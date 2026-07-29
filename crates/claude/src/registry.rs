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
        let Ok(entries) = std::fs::read_dir(&sessions) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_slice::<SessionEntry>(&bytes) else {
                continue;
            };
            if pid_alive(entry.pid) {
                out.push(LiveSession {
                    entry,
                    config_dir: dir.clone(),
                });
            }
        }
    }
    out
}

/// Is this claude session currently live in ANY of the given config dirs?
/// (Resuming it twice would interleave two writers into one transcript.)
pub fn session_is_live(config_dirs: impl IntoIterator<Item = PathBuf>, session_id: &str) -> bool {
    scan(config_dirs)
        .iter()
        .any(|s| s.entry.session_id == session_id)
}

/// Locate a session's transcript inside one config dir:
/// `projects/<munged-cwd>/<session_id>.jsonl`. The munging is lossy, so we
/// scan project dirs instead of reconstructing it.
pub fn find_transcript(config_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let projects = config_dir.join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The working directory a transcript's conversation ran in — the *only*
/// directory `claude --resume` will find it from. Early lines carry a `cwd`
/// field (format is internal; we scan a bounded prefix and tolerate misses).
pub fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(50) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str())
            && !cwd.is_empty()
        {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

/// Claude's project-dir name for a cwd: every non-alphanumeric byte becomes
/// `-`, case preserved (verified against 2.1.220 layouts).
pub fn munge_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A past conversation in some project dir, for the resume picker.
#[derive(Debug, Clone)]
pub struct PastSession {
    pub id: String,
    /// AI title / last prompt (best-effort from the transcript tail).
    pub title: String,
    pub path: PathBuf,
    pub config_dir: PathBuf,
    pub modified: Option<std::time::SystemTime>,
    /// Currently open in some terminal — resuming would corrupt it.
    pub live: bool,
}

/// List past sessions for `cwd` across config dirs, newest first (capped).
pub fn list_sessions(config_dirs: &[PathBuf], cwd: &Path) -> Vec<PastSession> {
    use std::collections::HashSet;
    let live_ids: HashSet<String> = scan(config_dirs.iter().cloned())
        .into_iter()
        .map(|s| s.entry.session_id)
        .collect();
    let munged = munge_cwd(cwd);
    let mut out: Vec<PastSession> = Vec::new();
    for dir in config_dirs {
        let proj = dir.join("projects").join(&munged);
        let Ok(entries) = std::fs::read_dir(&proj) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if id.len() != 36 {
                continue;
            }
            let modified = e.metadata().ok().and_then(|m| m.modified().ok());
            out.push(PastSession {
                live: live_ids.contains(&id),
                id,
                title: String::new(),
                path,
                config_dir: dir.clone(),
                modified,
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out.truncate(15);
    for s in &mut out {
        s.title = tail_title(&s.path).unwrap_or_else(|| s.id[..8].to_string());
    }
    out
}

/// Best-effort session title from the transcript's tail: the last `aiTitle`
/// line, else the last `lastPrompt` (truncated).
fn tail_title(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 128 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = String::new();
    file.take(TAIL).read_to_string(&mut buf).ok()?;

    let mut title: Option<String> = None;
    let mut prompt: Option<String> = None;
    for line in buf.lines() {
        if !line.contains("\"aiTitle\"") && !line.contains("\"lastPrompt\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
            title = Some(t.to_string());
        } else if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str()) {
            prompt = Some(p.to_string());
        }
    }
    let mut best = title.or(prompt)?;
    best = best.replace(['\n', '\r'], " ");
    if best.chars().count() > 60 {
        best = best.chars().take(59).collect::<String>() + "…";
    }
    (!best.is_empty()).then_some(best)
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
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Field 4 (ppid) comes after the parenthesized comm, which may itself
        // contain spaces/parens — split after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else {
            return false;
        };
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

    #[test]
    fn munge_matches_claude_layout() {
        assert_eq!(
            munge_cwd(Path::new("/home/yoz/Dev/claude_test")),
            "-home-yoz-Dev-claude-test",
            "underscores and slashes become dashes, case preserved"
        );
        assert_eq!(
            munge_cwd(Path::new("/home/yoz/Dev/yoav.xyz.next")),
            "-home-yoz-Dev-yoav-xyz-next"
        );
    }

    #[test]
    fn lists_sessions_with_tail_titles() {
        let dir = std::env::temp_dir().join(format!("giverny-list-{}", std::process::id()));
        let cwd = Path::new("/home/u/proj_x");
        let proj = dir.join("projects").join(munge_cwd(cwd));
        std::fs::create_dir_all(&proj).unwrap();
        let sid_a = "aaaaaaaa-1111-2222-3333-444444444444";
        let sid_b = "bbbbbbbb-1111-2222-3333-444444444444";
        std::fs::write(
            proj.join(format!("{sid_a}.jsonl")),
            "{\"type\":\"ai-title\",\"aiTitle\":\"fix auth bug\",\"sessionId\":\"a\"}\n",
        )
        .unwrap();
        std::fs::write(
            proj.join(format!("{sid_b}.jsonl")),
            "{\"type\":\"last-prompt\",\"lastPrompt\":\"run the tests\",\"sessionId\":\"b\"}\n",
        )
        .unwrap();
        std::fs::write(proj.join("not-a-session.jsonl"), "junk").unwrap();

        let sessions = list_sessions(std::slice::from_ref(&dir), cwd);
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        let a = sessions.iter().find(|s| s.id == sid_a).unwrap();
        assert_eq!(a.title, "fix auth bug");
        let b = sessions.iter().find(|s| s.id == sid_b).unwrap();
        assert_eq!(b.title, "run the tests");
        assert!(!a.live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_transcript_and_its_cwd() {
        let dir = std::env::temp_dir().join(format!("giverny-transcript-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-u-Dev-myproj");
        std::fs::create_dir_all(&proj).unwrap();
        let sid = "b263c7bf-2cc6-4ee1-b00a-948a4152f6ab";
        std::fs::write(
            proj.join(format!("{sid}.jsonl")),
            concat!(
                "{\"mode\":\"default\",\"sessionId\":\"x\",\"type\":\"mode\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/home/u/Dev/myproj\",\"sessionId\":\"x\"}\n",
            ),
        )
        .unwrap();

        let found = find_transcript(&dir, sid).expect("transcript located");
        assert_eq!(
            transcript_cwd(&found),
            Some(PathBuf::from("/home/u/Dev/myproj")),
            "cwd read from early transcript lines"
        );
        assert!(find_transcript(&dir, "0000-not-there").is_none());
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
