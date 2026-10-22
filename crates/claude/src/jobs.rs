//! Background agents: the Claudes with no tab.
//!
//! Claude Code runs work that outlives the session that started it — `/fork`
//! copies a conversation into a background session, the Bash tool runs
//! commands with `run_in_background`, agents keep going after you move on.
//! Each writes `jobs/<id>/state.json` under its config dir, and the daemon
//! lists the ones it is actually running in `daemon/roster.json`.
//!
//! Without this, the one Claude you are most likely to forget is the only one
//! Giverny cannot see.
//!
//! Everything here is Claude Code's internal state and will churn: bind the
//! few fields that carry meaning, tolerate the rest being absent, and never
//! fail a scan because one file surprised us.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// The states a job reports. Taken from real timelines: `working`, `blocked`,
/// `done` — anything else is carried through as-is rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Doing something.
    Working,
    /// Waiting on a person — the reason this feature exists.
    Blocked,
    Done,
    Unknown,
}

impl JobState {
    fn parse(s: &str) -> JobState {
        match s {
            "working" | "running" => JobState::Working,
            "blocked" | "waiting" => JobState::Blocked,
            "done" | "completed" | "finished" => JobState::Done,
            _ => JobState::Unknown,
        }
    }

    pub fn needs_you(self) -> bool {
        self == JobState::Blocked
    }
}

/// Pull the fields out of a `serde_json::Value` rather than deriving onto a
/// struct.
///
/// Learned the hard way: `updatedAt` is an ISO-8601 *string*, not the number a
/// reasonable person assumes, and with a derived struct one wrong type makes
/// serde reject the whole record — the job vanished from the list with no
/// error anywhere. This is Claude Code's private state and it will keep
/// changing shape, so a surprise must cost one field, never the job.
fn as_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(Into::into)
}

fn as_u32(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as u32
}

/// Milliseconds since the epoch, from a number or an RFC 3339 string.
fn as_millis(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s
            .parse::<jiff::Timestamp>()
            .map(|t| t.as_millisecond().max(0) as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    /// Short id, and the directory name: `df1a1071`.
    pub id: String,
    /// The agent's name when it has one, else its id.
    pub name: String,
    pub state: JobState,
    pub detail: Option<String>,
    pub tasks: u32,
    pub queued: u32,
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
    pub resume_session_id: Option<String>,
    pub updated_at_ms: u64,
    /// Which account it belongs to.
    pub config_dir: PathBuf,
    /// The daemon is running a worker for it right now.
    pub live: bool,
    pub pinned: bool,
}

impl Job {
    /// Is this agent actually there to watch?
    ///
    /// Only one the daemon is running a live worker for. A `state.json`
    /// outlives the process that wrote it, so the directory fills up with
    /// agents that finished, crashed, or were last seen days ago — and a row
    /// reading "working" for something with no process is a lie that costs
    /// whoever clicks it a terminal with nothing in it.
    pub fn worth_watching(&self) -> bool {
        self.live && self.state != JobState::Done
    }

    /// What to resume to attach a tab to this agent.
    pub fn resume_target(&self) -> Option<&str> {
        self.resume_session_id
            .as_deref()
            .or(self.session_id.as_deref())
    }
}

#[derive(Debug, Deserialize)]
struct Roster {
    #[serde(default)]
    workers: HashMap<String, RosterWorker>,
}

#[derive(Debug, Deserialize)]
struct RosterWorker {
    #[serde(default)]
    pid: u32,
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        pid != 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        pid != 0 && sys.process(Pid::from_u32(pid)).is_some()
    }
}

/// Ids the daemon is currently running a worker for, with a live process.
fn live_ids(config_dir: &Path) -> Vec<String> {
    let path = config_dir.join("daemon").join("roster.json");
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(roster) = serde_json::from_slice::<Roster>(&bytes) else {
        return Vec::new();
    };
    roster
        .workers
        .into_iter()
        .filter(|(_, w)| pid_alive(w.pid))
        .map(|(id, _)| id)
        .collect()
}

fn pinned_ids(config_dir: &Path) -> Vec<String> {
    let path = config_dir.join("jobs").join("pins.json");
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .unwrap_or_default()
}

/// Every background job across the given accounts, newest activity first.
///
/// Finished jobs are kept: "it finished while I was away" is as much a thing
/// to notice as "it is still going". The caller decides how long to show them.
pub fn scan(config_dirs: impl IntoIterator<Item = PathBuf>) -> Vec<Job> {
    let mut out = Vec::new();
    for dir in config_dirs {
        let jobs_dir = dir.join("jobs");
        let Ok(entries) = std::fs::read_dir(&jobs_dir) else {
            continue;
        };
        let live = live_ids(&dir);
        let pinned = pinned_ids(&dir);
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue; // pins.json and friends
            }
            let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(path.join("state.json")) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let in_flight = v.get("inFlight").cloned().unwrap_or(Value::Null);
            out.push(Job {
                name: as_str(&v, "name").unwrap_or_else(|| id.clone()),
                state: JobState::parse(as_str(&v, "state").unwrap_or_default().as_str()),
                detail: as_str(&v, "detail"),
                tasks: as_u32(&in_flight, "tasks"),
                queued: as_u32(&in_flight, "queued"),
                cwd: as_str(&v, "cwd").map(PathBuf::from),
                session_id: as_str(&v, "sessionId"),
                resume_session_id: as_str(&v, "resumeSessionId"),
                updated_at_ms: as_millis(&v, "updatedAt"),
                live: live.contains(&id),
                pinned: pinned.contains(&id),
                config_dir: dir.clone(),
                id,
            });
        }
    }
    // Pinned first, then whoever moved most recently.
    out.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.updated_at_ms.cmp(&a.updated_at_ms))
    });
    out
}

/// Ending a background job, as Claude Code's own CLI does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// `claude stop <id>`: the worker goes, the conversation stays —
    /// `claude attach <id>` or `--resume` brings it back.
    Stop,
    /// `claude rm <id>`: the job and its worktree go for good.
    Delete,
}

impl End {
    fn subcommand(self) -> &'static str {
        match self {
            End::Stop => "stop",
            End::Delete => "rm",
        }
    }

    /// The word for it in the rail.
    pub fn verb(self) -> &'static str {
        match self {
            End::Stop => "stop",
            End::Delete => "delete",
        }
    }
}

/// Is `id` a job id, safe to hand to a command line? Claude Code's are
/// short hex; anything else came from a file that surprised us.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// End background job `id` of the account at `config_dir`.
///
/// Blocking — Claude Code talks to its daemon, which takes a moment — so
/// call it off the UI thread. Bounded: a CLI that never answers is killed,
/// not waited on. The error carries what the CLI said, for the row to show.
pub fn end(config_dir: &Path, id: &str, how: End) -> anyhow::Result<()> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    if !valid_id(id) {
        anyhow::bail!("not a job id: {id:?}");
    }
    let mut child = crate::usage::claude_command(config_dir)?
        .arg(how.subcommand())
        .arg(id)
        // Not a tab's claude: its hooks must not land on one.
        .env_remove("GIVERNY_TAB_ID")
        .env_remove("GIVERNY_NONCE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run claude: {e}"))?;
    // Read both pipes as the CLI writes, so a chatty answer cannot fill
    // one and stall the process the deadline is waiting on.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut text);
        }
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("claude {} {id} did not answer in 30 s", how.subcommand());
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let said = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let said = said.trim();
    if said.is_empty() {
        anyhow::bail!("claude {} {id} exited with {status}", how.subcommand());
    }
    anyhow::bail!("{said}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_job(config: &Path, id: &str, body: &str) {
        let dir = config.join("jobs").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), body).unwrap();
    }

    /// An id goes onto a command line: only what Claude Code's ids are
    /// made of gets there, and nothing runs for anything else.
    #[test]
    fn only_a_job_id_is_ended() {
        assert!(valid_id("1b3d5606"));
        assert!(valid_id("6e7e56e0-7c66"));
        for bad in ["", "a b", "x;rm -rf ~", "../jobs", "id\n"] {
            assert!(!valid_id(bad), "{bad:?}");
            let err = end(Path::new("/nonexistent"), bad, End::Stop).unwrap_err();
            assert!(err.to_string().starts_with("not a job id"), "{err}");
        }
        assert_eq!(End::Stop.subcommand(), "stop");
        assert_eq!(End::Delete.subcommand(), "rm");
    }

    #[test]
    fn reads_the_shape_claude_actually_writes() {
        // Trimmed from a real jobs/<id>/state.json.
        let config = scratch("real");
        write_job(
            &config,
            "df1a1071",
            r#"{
              "state": "working",
              "detail": "board drained; resume ACTIVE; next poll ~5 min",
              "tempo": "idle",
              "inFlight": { "tasks": 2, "queued": 0, "kinds": ["local_bash"] },
              "fan": [ { "id": "b0xpa7pas", "kind": "shell", "label": "until grep" } ],
              "sessionId": "df1a1071-0878-4f88-8a35-0900e709876f",
              "resumeSessionId": "aaaa1111-2222-3333-4444-555566667777",
              "cwd": "/home/yoz/Dev/hoteleak",
              "updatedAt": "2026-07-22T06:37:20.332Z"
            }"#,
        );
        let jobs = scan([config.clone()]);
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.state, JobState::Working);
        assert_eq!(job.tasks, 2);
        assert_eq!(job.name, "df1a1071", "no name field falls back to the id");
        assert!(job.detail.as_deref().unwrap().starts_with("board drained"));
        // updatedAt is an ISO-8601 string in the real files. A derived struct
        // expecting a number silently dropped the whole job.
        assert!(
            job.updated_at_ms > 1_700_000_000_000,
            "timestamp not parsed"
        );
        // Attaching a tab needs the resume id, not the session id.
        assert_eq!(
            job.resume_target(),
            Some("aaaa1111-2222-3333-4444-555566667777")
        );
        let _ = std::fs::remove_dir_all(&config);
    }

    /// The rail is for agents that are running right now: still working, or
    /// waiting on you. A finished one is neither, and neither is one whose
    /// process went away while its state file stayed behind.
    #[test]
    fn only_a_running_agent_is_worth_watching() {
        let config = scratch("finished");
        write_job(&config, "aaaa1111", r#"{ "state": "working" }"#);
        write_job(&config, "bbbb2222", r#"{ "state": "blocked" }"#);
        write_job(&config, "cccc3333", r#"{ "state": "done" }"#);
        write_job(&config, "dddd4444", r#"{ "state": "working" }"#);
        // The daemon is running workers for three of them; the fourth has a
        // state file and nothing behind it.
        let me = std::process::id();
        std::fs::create_dir_all(config.join("daemon")).unwrap();
        std::fs::write(
            config.join("daemon").join("roster.json"),
            format!(
                r#"{{"workers":{{"aaaa1111":{{"pid":{me}}},"bbbb2222":{{"pid":{me}}},
                    "cccc3333":{{"pid":{me}}}}}}}"#
            ),
        )
        .unwrap();
        let jobs = scan([config.clone()]);
        assert_eq!(jobs.len(), 4, "the scan still reports everything it finds");
        let mut watching: Vec<&str> = jobs
            .iter()
            .filter(|j| j.worth_watching())
            .map(|j| j.id.as_str())
            .collect();
        // Order is the scan's business (pinned first, then recency); this is
        // about which ones survive.
        watching.sort_unstable();
        assert_eq!(watching, ["aaaa1111", "bbbb2222"]);
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn blocked_is_the_state_that_wants_you() {
        assert!(JobState::parse("blocked").needs_you());
        assert!(!JobState::parse("working").needs_you());
        assert!(!JobState::parse("done").needs_you());
        // An unfamiliar state must not masquerade as an alarm.
        assert!(!JobState::parse("percolating").needs_you());
        assert_eq!(JobState::parse("percolating"), JobState::Unknown);
    }

    #[test]
    fn pinned_first_then_most_recently_active() {
        let config = scratch("order");
        // Numeric and string timestamps sort together — both shapes appear.
        write_job(&config, "old", r#"{"state":"done","updatedAt":100}"#);
        write_job(
            &config,
            "new",
            r#"{"state":"working","updatedAt":"2026-07-22T06:37:20.332Z"}"#,
        );
        write_job(&config, "pin", r#"{"state":"done","updatedAt":1}"#);
        std::fs::write(config.join("jobs").join("pins.json"), r#"["pin"]"#).unwrap();
        let ids: Vec<String> = scan([config.clone()]).into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["pin", "new", "old"]);
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn a_surprising_file_never_breaks_the_scan() {
        // This is Claude's internal state; it will change shape without notice.
        let config = scratch("junk");
        write_job(&config, "good", r#"{"state":"working","updatedAt":5}"#);
        write_job(&config, "bad", "{ not json");
        // A field of the wrong type costs that field, not the record.
        write_job(
            &config,
            "odd",
            r#"{"state":"working","updatedAt":{"nested":true}}"#,
        );
        std::fs::create_dir_all(config.join("jobs").join("empty")).unwrap();
        std::fs::write(config.join("jobs").join("pins.json"), "[]").unwrap();
        let jobs = scan([config.clone()]);
        assert_eq!(jobs.len(), 2, "only unparseable JSON is dropped");
        assert!(jobs.iter().any(|j| j.id == "good"));
        let odd = jobs.iter().find(|j| j.id == "odd").expect("odd survived");
        assert_eq!(odd.state, JobState::Working, "the good fields still read");
        assert_eq!(odd.updated_at_ms, 0, "the bad field degrades to zero");
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn no_jobs_directory_is_normal() {
        assert!(scan([scratch("absent")]).is_empty());
    }
}
