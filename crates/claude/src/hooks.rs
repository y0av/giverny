//! Claude Code hook relay: `giverny relay` (registered as a hook command)
//! forwards each hook payload — plus the tab identity it inherited from the
//! environment — to the app's unix socket, spooling to disk when the app
//! is closed. Also the settings.json installer.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Hook events Giverny consumes.
pub const RELAY_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    // Every tool call, which is the only thing that says "still working"
    // during a turn nobody prompted — a session carrying on after a
    // permission was granted, or an agent running itself. Without it a tab
    // keeps whatever the last turn's `Stop` left on it, and shows a tick
    // while it works.
    "PostToolUse",
    // A compaction starting. `/compact` sends no `UserPromptSubmit`, so
    // without it the tab reads idle through the minute it compacts.
    "PreCompact",
    "Stop",
    // A turn ended on an API error, which Claude Code sends in place of
    // `Stop`: the rail marks it with a ✕, not the tick.
    "StopFailure",
    "Notification",
    "SessionEnd",
];

/// Do any of our entries exist in this file, whatever the event?
///
/// The difference between "never installed" and "installed before this
/// version knew about a new event". The second one is ours to repair.
pub fn partly_installed_in(settings_path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    root.get("hooks")
        .and_then(|h| h.as_object())
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(|v| v.as_array())
                .flatten()
                .any(is_our_entry)
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayMsg {
    #[serde(default = "claude_provider")]
    pub provider: String,
    #[serde(default)]
    pub wsl_distro: Option<String>,
    #[serde(default)]
    pub emitted_at: Option<u64>,
    /// `$GIVERNY_TAB_ID` as inherited by the hook (absent outside Giverny).
    pub tab_id: Option<String>,
    /// The background job whose session ran the hook, by short id, in place
    /// of a tab id ([`crate::lineage::bg_job`]): the app sends it to the tab
    /// parked on that job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// `$CLAUDE_CONFIG_DIR` — which account profile the session runs under.
    pub config_dir: Option<String>,
    /// The raw hook payload.
    pub event: serde_json::Value,
}

fn claude_provider() -> String {
    "claude".into()
}

impl RelayMsg {
    pub fn hook_event(&self) -> Option<&str> {
        self.event.get("hook_event_name").and_then(|v| v.as_str())
    }
    pub fn session_id(&self) -> Option<&str> {
        self.event.get("session_id").and_then(|v| v.as_str())
    }
    pub fn notification_type(&self) -> Option<&str> {
        self.event.get("notification_type").and_then(|v| v.as_str())
    }
    pub fn message(&self) -> Option<&str> {
        self.event.get("message").and_then(|v| v.as_str())
    }
    /// What the user typed, on `UserPromptSubmit`.
    pub fn prompt(&self) -> Option<&str> {
        self.event.get("prompt").and_then(|v| v.as_str())
    }
    /// Was this hook fired by one of the session's subagents rather than the
    /// session itself? A subagent's tool hooks arrive under its parent's
    /// `session_id`, told apart only by the `agent_id` they carry; the
    /// session's own calls carry none (measured on a `PostToolUse`).
    pub fn from_subagent(&self) -> bool {
        self.agent_id().is_some_and(|a| !a.trim().is_empty())
    }

    /// The subagent a tool hook fired in. Claude Code (2.1.296) builds the
    /// tool hooks from the tool call's context, so a subagent's tool calls
    /// arrive under the parent's `session_id` with this set.
    /// `UserPromptSubmit` never carries it.
    pub fn agent_id(&self) -> Option<&str> {
        self.event.get("agent_id").and_then(|v| v.as_str())
    }

    /// The subagent whose final report this tool call delivered: a
    /// `PostToolUse` of `SubagentHandback` from a subagent. Its last call —
    /// the worker has ended (Claude Code 2.1.302).
    pub fn handed_back(&self) -> Option<&str> {
        let tool = self.event.get("tool_name").and_then(|v| v.as_str());
        (tool == Some(HANDBACK_TOOL))
            .then(|| self.agent_id())
            .flatten()
            .filter(|a| !a.trim().is_empty())
    }

    /// On the session's own `PostToolUse` of `TaskStop`: the background task
    /// it stopped.
    pub fn stopped_task(&self) -> Option<&str> {
        if self.event.get("tool_name").and_then(|v| v.as_str()) != Some("TaskStop") {
            return None;
        }
        let input = self.event.get("tool_input")?;
        input
            .get("task_id")
            .or_else(|| input.get("shell_id"))
            .and_then(|v| v.as_str())
    }

    /// On `Stop`: the ids of the delegated work [`Self::delegated_work_running`]
    /// counts, and whether any of it came without one.
    pub fn delegated_ids(&self) -> (Vec<&str>, bool) {
        let mut ids = Vec::new();
        let mut unnamed = false;
        for task in in_flight(&self.event).filter(|task| {
            task.get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| DELEGATED.contains(&t))
        }) {
            match task.get("id").and_then(|v| v.as_str()) {
                Some(id) if !id.is_empty() => ids.push(id),
                _ => unnamed = true,
            }
        }
        (ids, unnamed)
    }

    /// On `Stop`: is work this session delegated still running as the turn
    /// ends — a subagent, a workflow, a teammate, a cloud session?
    ///
    /// Claude Code hands every `Stop` hook a `background_tasks` list, its
    /// in-flight background work (running or pending), "to distinguish
    /// session is done from session is paused waiting for background work"
    /// (2.1.296). Each entry's `type` is a friendly label (`subagent`,
    /// `shell`, `monitor`, …) or, for a type it has no label for, the raw
    /// one; both spellings of the four kinds its own session status counts
    /// as working are taken here. A background shell is not: the registry's
    /// `shell` already says that one, and a monitor or a cron
    /// is a watcher that may never end. No list (an older Claude Code): no.
    ///  The list is read by [`in_flight`].
    pub fn delegated_work_running(&self) -> bool {
        in_flight(&self.event).any(|task| {
            task.get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| DELEGATED.contains(&t))
        })
    }
}

/// The `background_tasks` types that are work a session delegated — a
/// subagent, a workflow, a teammate, a cloud session — in both spellings:
/// the friendly label Claude Code gives a type it knows and the raw type.
pub const DELEGATED: &[&str] = &[
    "subagent",
    "local_agent",
    "workflow",
    "local_workflow",
    "teammate",
    "in_process_teammate",
    "cloud session",
    "remote_agent",
];

/// The tool a subagent delivers its final report with.
pub const HANDBACK_TOOL: &str = "SubagentHandback";

/// The tasks a turn's injected `<task-notification>`s say have ended: the
/// `<task-id>` of each that is `completed`, `failed` or `killed` and not
/// merely paused. Claude Code marks a subagent `completed` as soon as it
/// stops with background work of its own still running — "is waiting on its
/// own background work", to resume when that work reports — and drops it
/// from every later `Stop`'s `background_tasks` while it is still alive
/// (2.1.302). Only each notification's head is read: its
/// `<result>` is the task's own output, model text that could spell anything.
pub fn ended_tasks(prompt: &str) -> Vec<&str> {
    const OPEN: &str = "<task-notification>";
    const CLOSE: &str = "</task-notification>";
    fn tag<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let start = head.find(&open)? + open.len();
        let len = head[start..].find(&close)?;
        Some(head[start..start + len].trim())
    }
    let mut out = Vec::new();
    let mut rest = prompt;
    while let Some(at) = rest.find(OPEN) {
        let block = &rest[at + OPEN.len()..];
        let end = block.find(CLOSE).unwrap_or(block.len());
        let mut head = &block[..end];
        if let Some(result) = head.find("<result>") {
            head = &head[..result];
        }
        rest = &block[end..];
        let ended = matches!(tag(head, "status"), Some("completed" | "failed" | "killed"));
        let paused = head.contains("waiting on its own background work")
            || head.contains("background work of its own still running");
        if ended
            && !paused
            && let Some(id) = tag(head, "task-id").filter(|id| !id.is_empty())
        {
            out.push(id);
        }
    }
    out
}

/// The `background_tasks` types that are commands rather than agents: a
/// background shell (a Bash `run_in_background`, or one the person sent to
/// the background) and a monitor, both spellings.
pub const COMMANDS: &[&str] = &[
    "shell",
    "local_bash",
    "monitor",
    "monitor_mcp",
    "monitor_ws",
];

/// A `Stop` or `SubagentStop` payload's `background_tasks` still in flight:
/// running or pending, or with no status at all. No list (a Claude Code
/// before 2.1.296): none. Claude Code fills the list from the session's one
/// task registry, so a subagent's `SubagentStop` lists its parent's tasks
/// and its own alike, the subagent itself among them, and no entry says
/// whose it is (measured on 2.1.296).
pub fn in_flight(event: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    event
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|task| {
            task.get("status")
                .and_then(|v| v.as_str())
                .is_none_or(|s| matches!(s, "running" | "pending"))
        })
}

pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("giverny.sock");
    }
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir().join(format!("giverny-{uid}.sock"))
}

/// Who a hook's message is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The tab the session runs in (`$GIVERNY_TAB_ID`).
    Tab(String),
    /// A background job's session, by short id: the tab showing it is the
    /// app's to find. Its inherited tab id is the daemon's, not the tab's.
    Job(String),
}

impl Target {
    /// This hook's addressee, or `None` when there is no one to tell:
    /// outside Giverny, or in a session that only inherited a tab's identity
    /// — a claude started inside the tab's claude, or under a multiplexer
    /// the tab launched — whose hooks would move the tab's state and its
    /// resume target.
    pub fn of_this_process() -> Option<Target> {
        if let Some(job) = crate::lineage::bg_job() {
            return Some(Target::Job(job));
        }
        // A claude started inside a job's shell: no tab's, and the tab id it
        // inherited is the daemon's.
        let tab = crate::lineage::giverny_var("GIVERNY_TAB_ID")?;
        crate::lineage::of_this_process()
            .is_tabs()
            .then_some(Target::Tab(tab))
    }

    /// Which account the session runs under. A background job's tab
    /// variables are the daemon's, so its `GIVERNY_PROFILE_DIR` may name
    /// another Giverny's view of the account; its job directory,
    /// `<config>/jobs/<id>`, names the account itself.
    pub fn account(&self) -> Option<String> {
        match self {
            Target::Tab(_) => account_dir(),
            Target::Job(_) => account_dir(),
        }
    }

    /// A message to the app addressed to this target.
    pub fn msg(self, config_dir: Option<String>, event: serde_json::Value) -> RelayMsg {
        let (tab_id, job) = match self {
            Target::Tab(tab) => (Some(tab), None),
            Target::Job(job) => (None, Some(job)),
        };
        RelayMsg {
            provider: claude_provider(),
            wsl_distro: None,
            emitted_at: None,
            tab_id,
            job,
            config_dir,
            event,
        }
    }
}

/// The `giverny relay` entrypoint. Fast, silent, always exits successfully —
/// a relay failure must never disturb the Claude session that ran the hook.
pub fn run_relay(spool: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1_000_000).read_to_string(&mut input);
    // Nothing to relay outside a tab or a background job (and nothing worth
    // spooling; the stdin read above keeps claude's pipe-write happy before
    // we bail).
    let Some(target) = Target::of_this_process() else {
        return;
    };
    let event: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    let arg = |name: &str| std::env::args().skip_while(|s| s != name).nth(1);
    let codex = arg("--provider").as_deref() == Some("codex");
    let config_dir = if codex {
        arg("--home").or_else(|| std::env::var("CODEX_HOME").ok())
    } else {
        target.account()
    };
    let mut msg = target.msg(config_dir, event);
    if codex {
        msg.provider = "codex".into();
    }
    msg.wsl_distro = arg("--wsl-distro");
    msg.emitted_at = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
    );
    deliver(&msg, spool);
}

/// Which account this session runs under.
///
/// `CLAUDE_CONFIG_DIR` is the truth when it is set — the user may have named
/// a different account inside the tab, and that is the one the hook belongs
/// to. When it is unset the session is on its default account, which the
/// session itself cannot name: `GIVERNY_PROFILE_DIR` is the tab telling us
/// which one that is, and inside WSL it is the only way the answer crosses
/// back at all.
///
/// Inside a background job the tab's word is the daemon's, so the job's
/// directory, `<config>/jobs/<id>`, names the account instead.
fn account_dir() -> Option<String> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .or_else(|| crate::lineage::job_account().map(|d| d.display().to_string()))
        .or_else(|| crate::lineage::giverny_var("GIVERNY_PROFILE_DIR"))
}

/// Send one message to the app: unix socket when available, else append to
/// the spool file. A running app polls the spool; a closed one drains it at
/// next launch, so session-id captures are never lost.
fn deliver(msg: &RelayMsg, spool: &Path) {
    let Ok(line) = serde_json::to_string(msg) else {
        return;
    };

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        if let Ok(mut stream) = UnixStream::connect(socket_path()) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            if writeln!(stream, "{line}").is_ok() {
                return;
            }
        }
    }
    if let Some(dir) = spool.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spool)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Synthetic event name for `giverny manage clear-done` (not a Claude hook
/// event): clear the management panel's Done rows in the tab it ran in.
/// The event carries `session_id` and `at_ms`, the moment the
/// command ran.
pub const CLEAR_DONE_EVENT: &str = "GivernyClearDone";

/// Ask the app to clear the management panel's Done rows in the tab this process
/// runs in (`$GIVERNY_TAB_ID`). `false` outside a Giverny tab, where there is
/// no pane to clear. A closed app gets it from the spool at its next launch.
pub fn send_clear_done(spool: &Path, session: Option<&str>, at_ms: u64) -> bool {
    // Run from a background job's shell, the command is the job's: its
    // tab id is the daemon's.
    let target = match crate::lineage::bg_job() {
        Some(job) => Target::Job(job),
        None => match crate::lineage::giverny_var("GIVERNY_TAB_ID") {
            Some(tab) => Target::Tab(tab),
            None => return false,
        },
    };
    let event = serde_json::json!({
        "hook_event_name": CLEAR_DONE_EVENT,
        "session_id": session,
        "at_ms": at_ms,
    });
    let account = target.account();
    deliver(&target.msg(account, event), spool);
    true
}

/// Drain and clear the spool file, returning whatever it held.
fn drain_spool(spool: &Path) -> Vec<RelayMsg> {
    let mut out = Vec::new();
    if let Ok(content) = std::fs::read_to_string(spool) {
        for line in content.lines() {
            if let Ok(msg) = serde_json::from_str::<RelayMsg>(line) {
                out.push(msg);
            }
        }
        let _ = std::fs::remove_file(spool);
    }
    out
}

/// Spool-file transport: the relay appends lines, the app polls and drains.
/// This is the Windows path (no unix sockets) and the fallback anywhere the
/// socket cannot be bound. Latency is the poll interval, not instant, but it
/// needs no IPC primitives at all.
pub fn spawn_spool_watcher(
    spool: &Path,
    wake: impl Fn() + Send + 'static,
) -> anyhow::Result<(crossbeam_channel::Receiver<RelayMsg>, Vec<RelayMsg>)> {
    let spooled = drain_spool(spool);
    let (tx, rx) = crossbeam_channel::unbounded();
    let path = spool.to_path_buf();
    std::thread::Builder::new()
        .name("giverny hook spool".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(400));
                let batch = drain_spool(&path);
                if batch.is_empty() {
                    continue;
                }
                for msg in batch {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                wake();
            }
        })?;
    Ok((rx, spooled))
}

/// Bind the app-side listener. `wake` is called after each delivered message
/// (the app passes a repaint trigger). Returns the receiver plus any messages
/// spooled while the app was closed.
///
/// The socket is also how one Giverny finds another: a second launch sends
/// [`RAISE_REQUEST`] instead of opening a window ([`ask_running_to_raise`]).
/// Each such request reaches `raise` as the connection to answer, and the app
/// answers it from its UI thread, so an answer means a window that is alive.
#[cfg(unix)]
pub fn spawn_listener(
    spool: &Path,
    raise: crossbeam_channel::Sender<std::os::unix::net::UnixStream>,
    wake: impl Fn() + Send + 'static,
) -> anyhow::Result<(crossbeam_channel::Receiver<RelayMsg>, Vec<RelayMsg>)> {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    let spooled = drain_spool(spool);

    let path = socket_path();
    // A socket that still accepts connections belongs to a live instance —
    // unlinking and rebinding would silently steal its hook stream. Only a
    // stale socket (owner gone) may be replaced.
    if let Some(holder) = socket_holder(&path) {
        return Err(Held { path, holder }.into());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let (tx, rx) = crossbeam_channel::unbounded();

    std::thread::Builder::new()
        .name("giverny hook listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let reader = std::io::BufReader::new(&stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if is_raise_request(&line) {
                        if let Ok(reply) = stream.try_clone() {
                            let _ = raise.send(reply);
                            wake();
                        }
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<RelayMsg>(&line) {
                        let _ = tx.send(msg);
                        wake();
                    }
                }
            }
        })?;

    Ok((rx, spooled))
}

/// What a second launch sends down the relay socket in place of a hook: come
/// to the front, because the user just started Giverny again.
pub const RAISE_REQUEST: &str = r#"{"giverny":"raise"}"#;

#[cfg(unix)]
fn is_raise_request(line: &str) -> bool {
    #[derive(Deserialize)]
    struct Control {
        giverny: String,
    }
    serde_json::from_str::<Control>(line).is_ok_and(|c| c.giverny == "raise")
}

/// The answer to a [`RAISE_REQUEST`], written by the running app once its
/// window has been told to come forward.
pub fn raise_reply() -> String {
    format!("{{\"raised\":true,\"pid\":{}}}\n", std::process::id())
}

/// Whoever already listens on the relay socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    /// Its process id, where the platform tells (Linux).
    pub pid: Option<u32>,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "another Giverny (pid {pid})"),
            None => write!(f, "another Giverny"),
        }
    }
}

/// The relay socket is taken by a live Giverny, which gets the hooks.
#[derive(Debug)]
pub struct Held {
    pub path: PathBuf,
    pub holder: Holder,
}

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} holds the relay at {} — this window will not receive hook events",
            self.holder,
            self.path.display()
        )
    }
}

impl std::error::Error for Held {}

/// Who listens on the socket at `path`, if anyone does. A socket file whose
/// owner is gone refuses the connection, and counts as nobody.
#[cfg(unix)]
pub fn socket_holder(path: &Path) -> Option<Holder> {
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    Some(Holder {
        pid: peer_pid(&stream),
    })
}

/// The pid of the process at the other end of a unix socket: for a client's
/// end, the one that is listening.
#[cfg(target_os = "linux")]
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a live fd, and a buffer of the size we pass.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn peer_pid(_stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

/// What a second launch found on the relay socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Running {
    /// Nobody listens: this launch is the one Giverny.
    Nobody,
    /// A Giverny answered and is coming to the front.
    Raised { pid: Option<u32> },
    /// Something holds the socket but gave no answer in time: a window that
    /// is hung, or a Giverny too old to know the request.
    Silent(Holder),
}

/// Ask the Giverny that holds the relay socket, if one does, to come to the
/// front. The answer comes from that app's UI thread, so `timeout` is how
/// long a live window may take to get to it.
#[cfg(unix)]
pub fn ask_running_to_raise(timeout: Duration) -> Running {
    ask_to_raise(&socket_path(), timeout)
}

#[cfg(unix)]
fn ask_to_raise(path: &Path, timeout: Duration) -> Running {
    use std::io::BufRead;
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(path) else {
        return Running::Nobody;
    };
    let holder = Holder {
        pid: peer_pid(&stream),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_read_timeout(Some(timeout));
    if writeln!(stream, "{RAISE_REQUEST}").is_err() {
        return Running::Silent(holder);
    }
    // Done writing: the listener reads to the end of a connection before it
    // takes the next one.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut answer = String::new();
    let _ = std::io::BufReader::new(&stream).read_line(&mut answer);
    #[derive(Deserialize)]
    struct Reply {
        raised: bool,
        pid: Option<u32>,
    }
    match serde_json::from_str::<Reply>(answer.trim()) {
        Ok(Reply { raised: true, pid }) => Running::Raised {
            pid: pid.or(holder.pid),
        },
        _ => Running::Silent(holder),
    }
}

// ---- settings.json installer ----------------------------------------------

/// The hook command for this running binary.
pub fn relay_command() -> String {
    format!("{} relay", exe_path())
}

/// The hook command to write into one account's `settings.json`.
///
/// A settings file inside a WSL distribution is read by a Claude Code running
/// inside it, so the command has to name something that distribution can
/// execute. It can execute this very binary — Windows programs run from WSL
/// through interop — and a relay running as a Windows process then writes to
/// the same spool the app already watches, with no second transport to build.
/// What does not cross by itself is the environment: `GIVERNY_TAB_ID` reaches
/// it through `WSLENV`, which the tab sets when it spawns.
pub fn relay_command_for(settings_path: &Path) -> String {
    format!("{} relay", exe_for(settings_path))
}

/// The statusline command for one account's `settings.json`.
pub fn statusline_command_for(settings_path: &Path) -> String {
    format!("{} statusline", exe_for(settings_path))
}

fn exe_path() -> String {
    #[cfg(unix)]
    if let Some(link) = link_path()
        && is_link_to_a_binary(&link)
    {
        return command_word(&link);
    }
    running_exe()
        .map(|p| command_word(&p))
        .unwrap_or_else(|| "giverny".into())
}

/// A path as the first word of a hook command. On macOS the link lives
/// under `~/Library/Application Support`, and an unquoted space there split
/// every hook and the status line into a command that does not exist.
fn command_word(path: &Path) -> String {
    word_for_shell(&path.display().to_string(), cfg!(windows))
}

/// Claude Code on Windows runs hook commands with Git Bash, or PowerShell
/// where Git Bash is not installed. Bash takes `\` as an escape, so
/// `C:\Users\…` arrives as `C:Users…`; forward slashes run in both shells,
/// and need no quoting unless the path has a space. A quoted path only
/// suits Git Bash (PowerShell wants `& '…'`), but it is the default.
fn word_for_shell(path: &str, windows: bool) -> String {
    if windows {
        shell_quote(&path.replace('\\', "/"))
    } else {
        shell_quote(path)
    }
}

/// This binary's path. Linux names a binary that was rebuilt in place while
/// it ran `<path> (deleted)` — a path nothing can run, and once it reached
/// an account's `settings.json` from here.
pub fn running_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(
        match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(live) => PathBuf::from(live),
            None => exe,
        },
    )
}

/// The one path every account's hooks, status lines and plugin name for
/// Giverny: `<giverny config dir>/bin/giverny`, a link each Giverny points at
/// itself when it starts ([`point_link`]).
///
/// Naming the running binary instead made `settings.json` follow whichever
/// Giverny started last — a rebuild elsewhere, a test build, a reinstall —
/// and Claude Code reads hooks when a session starts, so every rewrite left
/// the running sessions behind. Through the link, a new binary changes the
/// link and never the settings, and a running session's next hook runs
/// whichever Giverny is current.
#[cfg(unix)]
pub fn link_path() -> Option<PathBuf> {
    // Giverny's config dir, as `giverny_core::state::Paths` finds it.
    let base = dirs::config_dir().or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("giverny").join("bin").join("giverny"))
}

/// A link (not a file of its own) whose target is there to run.
#[cfg(unix)]
fn is_link_to_a_binary(link: &Path) -> bool {
    std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::metadata(link).is_ok_and(|m| m.is_file())
}

/// Does [`link_path`] name a binary that is gone? A build that pointed it at
/// itself and was then deleted (`cargo clean`) leaves every session's hooks
/// running nothing, until a Giverny points it somewhere real again.
#[cfg(unix)]
pub fn link_is_dangling() -> bool {
    link_path().is_some_and(|link| is_dangling(&link))
}

#[cfg(unix)]
fn is_dangling(link: &Path) -> bool {
    std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink())
        && !std::fs::metadata(link).is_ok_and(|m| m.is_file())
}

/// Point [`link_path`] at this binary, so the commands written into each
/// account name it. Done at startup by the Giverny that looks after the
/// accounts; a side instance (`GIVERNY_NO_ACCOUNT_SETUP`) leaves it alone.
#[cfg(unix)]
pub fn point_link() -> std::io::Result<()> {
    let (Some(link), Some(exe)) = (link_path(), running_exe()) else {
        return Ok(());
    };
    point_link_at(&link, &exe)
}

#[cfg(unix)]
fn point_link_at(link: &Path, exe: &Path) -> std::io::Result<()> {
    // The binary itself, not a path to it: Giverny started through the link
    // (macOS reports the path it was started by) would otherwise point the
    // link at itself, and every hook would run nothing.
    // A binary copied to the link's own path is left alone the same way.
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    if exe == link || std::fs::read_link(link).is_ok_and(|t| t == exe) {
        return Ok(());
    }
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Beside it, then over it: a hook running meanwhile finds the old
    // binary or the new one, never no file at all.
    let tmp = link.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&exe, &tmp)?;
    std::fs::rename(&tmp, link)
}

/// How this binary is named to whoever will run the hook.
fn exe_for(settings_path: &Path) -> String {
    #[cfg(windows)]
    if let Some((distro, _)) = crate::wsl::split_unc(settings_path)
        && let Ok(exe) = std::env::current_exe()
        && let Some(inside) = crate::wsl::to_wsl_path(&distro, &exe)
    {
        return shell_quote(&inside);
    }
    let _ = settings_path;
    exe_path()
}

/// A path as one word for the shell Claude Code runs hook commands with.
/// Windows paths under `/mnt/c` land in `Program Files`, and macOS's config
/// dir in `Application Support`, so this is not hypothetical.
fn shell_quote(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '~'))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// How often Claude Code reruns our status line, seconds, besides its own
/// events: each run pushes the session's usage to the app and tells a
/// cache that has gone cold.
pub const STATUSLINE_REFRESH_S: u64 = 1;

/// Synthetic event name for statusline pushes (not a Claude hook event).
pub const STATUSLINE_EVENT: &str = "GivernyStatusLine";

/// The `giverny statusline` entrypoint: Claude Code runs this after every
/// assistant message and displays our stdout. We forward the payload's
/// official `rate_limits` to the app (fresh usage without any API call) and
/// print a compact line back: model, token counts and a cold cache. The
/// session's CPU and memory are not on it: Giverny's sidebar
/// and management panel show them.
pub fn run_statusline(spool: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1_000_000).read_to_string(&mut input);
    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);

    let mut event = serde_json::Map::new();
    event.insert(
        "hook_event_name".into(),
        serde_json::Value::String(STATUSLINE_EVENT.into()),
    );
    if let Some(limits) = payload.get("rate_limits") {
        event.insert("rate_limits".into(), limits.clone());
    }
    if let Some(sid) = payload.get("session_id") {
        event.insert("session_id".into(), sid.clone());
    }
    // Usage is no tab's, so a session the relay would not speak for still
    // delivers it; a background job's says which job it is.
    let target = Target::of_this_process();
    // The worker whose view the tab shows, as the app tells it:
    // only the tab's own claude's line is about it, not one that inherited it.
    let viewing = match &target {
        Some(Target::Tab(tab)) => viewing_of(spool, tab),
        _ => None,
    };
    let msg = match target {
        Some(target) => {
            let account = target.account();
            target.msg(account, serde_json::Value::Object(event))
        }
        None => RelayMsg {
            provider: claude_provider(),
            wsl_distro: None,
            emitted_at: None,
            tab_id: crate::lineage::giverny_var("GIVERNY_TAB_ID"),
            job: None,
            config_dir: account_dir(),
            event: serde_json::Value::Object(event),
        },
    };
    deliver(&msg, spool);

    let now_ms = jiff::Timestamp::now().as_millisecond();
    println!("{}", statusline_text(&payload, viewing.as_ref(), now_ms));
}

/// What Claude Code displays for the status line payload `payload` at
/// `now_ms`. Keep it short and useful: the model with the session's name,
/// its tokens, and a cache gone cold. No 5h or week usage, and no CPU,
/// memory or GPU: the sidebar shows the usage, and its tab
/// rows and category headers and the management panel's rows the use.
///
/// `viewing` is the worker whose view the tab shows ([`viewing_of`]):
/// while it is on screen the whole line is that worker's
/// ([`worker_statusline`]). Claude Code builds the payload from the main
/// session whatever view is shown, so nothing in `payload` says which one is.
fn statusline_text(payload: &serde_json::Value, viewing: Option<&Viewing>, now_ms: i64) -> String {
    if let Some(viewing) = viewing {
        return worker_statusline(payload, viewing, &crate::feed::feed_dir());
    }
    let mut parts: Vec<String> = Vec::new();
    let session_id = payload.get("session_id").and_then(|s| s.as_str());
    let transcript = transcript_of(payload);
    if let Some(model) = payload
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(|d| d.as_str())
    {
        let name = session_id
            .and_then(|s| crate::names::shown(&crate::feed::feed_dir(), s, transcript.as_deref()));
        parts.push(model_segment(model, name.as_deref(), || {
            accent_sgr(&statusline_accent_path())
        }));
    }
    parts.extend(statusline_tokens(payload, transcript.as_deref()));
    parts.extend(cache_cold_segment(payload, transcript.as_deref(), now_ms));
    parts.join(crate::session_use::SEP)
}

/// The status line while a worker's view is on screen: only
/// that worker, nothing of the whole session's.
///
/// * **Model:** the worker's, off its transcript's last reply
///   ([`crate::tokens::last_model`]), named the way Claude Code names the
///   main one (`Opus 5.5`, [`model_display`]); none until it has replied.
/// * **Name:** its task name only ([`worker_name`]), in the accent, where
///   the session's name stands on main.
/// * **Tokens:** the same segments the main session gets
///   ([`token_segments`]), counted off the worker's transcript alone:
///   `session:` is the context it carries now, `(+<n>)` what it spent before
///   its own compactions, `subagents:` the agents it spawned itself
///   (their own spawns too), and `total:` those added.
/// * **No cache-cold segment.** The main one is the manager's cache, and a
///   worker on view is running, so its own cache is warm.
fn worker_statusline(payload: &serde_json::Value, viewing: &Viewing, feeds: &Path) -> String {
    let session_id = payload.get("session_id").and_then(|s| s.as_str());
    let transcript = worker_transcript(viewing, transcript_of(payload).as_deref(), session_id);
    let name = worker_name(viewing, feeds, session_id);
    let model = transcript
        .as_deref()
        .and_then(crate::tokens::last_model)
        .map(|m| model_display(&m));
    let accent = || accent_sgr(&statusline_accent_path());
    let mut parts = vec![match model {
        Some(model) => model_segment(&model, Some(&name), accent),
        None => format!("{}({name}){RESET}", accent()),
    }];
    if let Some(t) = transcript.as_deref() {
        // `Null`: the payload's `context_window` is the manager's.
        let spawned = worker_subagents(t);
        parts.extend(token_segments(&serde_json::Value::Null, Some(t), &spawned));
    }
    parts.join(crate::session_use::SEP)
}

/// The transcripts of the agents the worker at `worker` (`agent-<id>.jsonl`)
/// spawned, and theirs in turn: beside it, by their metas' `parentAgentId`.
fn worker_subagents(worker: &Path) -> Vec<PathBuf> {
    use crate::subagents::{agent_transcript, list_agent_ids, read_meta};
    let (Some(dir), Some(id)) = (
        worker.parent(),
        worker
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("agent-")?.strip_suffix(".jsonl")),
    ) else {
        return Vec::new();
    };
    let parents: Vec<(String, Option<String>)> = list_agent_ids(dir)
        .into_iter()
        .map(|a| {
            let parent = read_meta(dir, &a).parent_id;
            (a, parent)
        })
        .collect();
    let mut family = vec![id.to_string()];
    let mut out = Vec::new();
    loop {
        let before = family.len();
        for (a, parent) in &parents {
            if !family.contains(a) && parent.as_ref().is_some_and(|p| family.contains(p)) {
                family.push(a.clone());
                out.push(agent_transcript(dir, a));
            }
        }
        if family.len() == before {
            return out;
        }
    }
}

/// The worker's `agent-<id>.jsonl`: the one the app named, else its agent
/// id's in any `subagents/` dir of the session.
fn worker_transcript(
    viewing: &Viewing,
    main: Option<&Path>,
    session_id: Option<&str>,
) -> Option<PathBuf> {
    if let Some(t) = viewing.transcript.as_ref().filter(|t| t.is_file()) {
        return Some(t.clone());
    }
    let id = viewing.agent_id.as_deref()?;
    crate::tokens::session_subagent_dirs(main, config_dir().as_deref(), session_id)
        .iter()
        .map(|dir| crate::subagents::agent_transcript(dir, id))
        .find(|p| p.is_file())
}

/// The worker's task name (`g477`), never its description: the one the app
/// knew from the management panel's row, else the manager's feed row its
/// agent id holds (a Running one first: a reused worker holds several),
/// else the head of a view label shaped `g477: what it does`, else `worker`.
fn worker_name(viewing: &Viewing, feeds: &Path, session_id: Option<&str>) -> String {
    let from_feed = || {
        let id = viewing.agent_id.as_deref()?;
        let (_, feed) = crate::feed::find(feeds, session_id?)?;
        let held: Vec<_> = feed
            .rows
            .iter()
            .filter(|r| r.agent_id.as_deref() == Some(id))
            // The `agent-<id>` key a worker with no task started its own
            // row under names nothing.
            .filter(|r| !(r.follows_worker && r.key.starts_with("agent-")))
            .collect();
        held.iter()
            .rfind(|r| r.stage() == crate::feed::Stage::Running)
            .or(held.last())
            .map(|r| r.key.clone())
    };
    viewing
        .task
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(from_feed)
        .or_else(|| viewing.label.as_deref().and_then(task_of_label))
        .unwrap_or_else(|| "worker".to_string())
}

/// The task name a label opens with: `g477` of `g477: one look`, or a label
/// that is one short word itself. Not a description.
fn task_of_label(label: &str) -> Option<String> {
    let head = label.split_once(':').map_or(label, |(h, _)| h).trim();
    (!head.is_empty() && head.chars().count() <= 24 && !head.contains(char::is_whitespace))
        .then(|| head.to_string())
}

/// A model id as Claude Code names it on the status line: `claude-opus-5-5`
/// → `Opus 5.5`, `claude-haiku-4-5-20251001` → `Haiku 4.5`. An id of no
/// known family is shown as it is.
fn model_display(id: &str) -> String {
    let base = id.split('[').next().unwrap_or(id).trim();
    let mut parts = base.strip_prefix("claude-").unwrap_or(base).split('-');
    let family = parts.next().unwrap_or_default();
    if !["opus", "sonnet", "haiku", "fable"].contains(&family) {
        return id.to_string();
    }
    let version: Vec<&str> = parts
        .take_while(|p| !p.is_empty() && p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    let mut name = family[..1].to_ascii_uppercase() + &family[1..];
    if !version.is_empty() {
        name += " ";
        name += &version.join(".");
    }
    name
}

/// Red `cache cold · next msg <n>` once the main conversation's prompt cache
/// has expired: `<n>` is what the next message re-caches.
/// Nothing while it is warm, or when the provider reports no cache tokens.
///
/// Claude Code's `prompt_cache` says so once this process has sent a
/// request; a reopened session has sent none, and carries no `prompt_cache`
/// until its first message — the very message the warning is for — so then
/// the transcript's last reply answers: its time, the TTL it wrote, its size.
fn cache_cold_segment(
    payload: &serde_json::Value,
    transcript: Option<&Path>,
    now_ms: i64,
) -> Option<String> {
    let recache = match payload.get("prompt_cache") {
        Some(cache) => {
            let flag = |key: &str| cache.get(key).and_then(|v| v.as_bool());
            if flag("caching_observed") != Some(true) || flag("warm") != Some(false) {
                return None;
            }
            cache.get("recache_tokens_if_cold").and_then(|v| v.as_u64())
        }
        None => {
            let last = crate::tokens::last_reply(transcript?)?;
            if now_ms < last.at_ms + last.ttl_ms? {
                return None;
            }
            last.recache
        }
    };
    let text = match recache {
        Some(n) => format!("cache cold · next msg {}", crate::tokens::fmt_tokens(n)),
        None => "cache cold".to_string(),
    };
    Some(format!("{RED}{text}{RESET}"))
}

/// SGR red and reset, around the status line's alarm segments.
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// The model's name, and beside it the session's full name (`giverny manage
/// name`) in the chrome's accent:
/// `Opus 5.5 (Rotem Giverny)`. `accent` is asked only when there is a name.
fn model_segment(model: &str, name: Option<&str>, accent: impl FnOnce() -> String) -> String {
    match name {
        Some(name) => format!("{model} {}({name}){RESET}", accent()),
        None => model.to_string(),
    }
}

/// What the app tells a tab's status line about the worker whose view its
/// Claude Code shows, as JSON in [`viewing_flag`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewing {
    /// Its task name, the management-panel row's key (`g477`), when the
    /// pane has a row for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Its Claude Code agent id: finds its transcript, and its feed row
    /// when `task` is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Its `agent-<id>.jsonl`, when the app knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
    /// The agent's own name, else the label Claude Code puts on the view
    /// (the Agent call's description): only its `g477:` head is ever shown
    /// ([`task_of_label`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The file the app leaves for tab `tab_id`'s status line while that tab's
/// Claude Code shows a worker's view: a [`Viewing`]. Beside
/// the spool, one per tab, like [`strip_flag`]; none while the tab is on the
/// manager's own view.
pub fn viewing_flag(spool: &Path, tab_id: &str) -> PathBuf {
    viewing_dir(spool).join(tab_id)
}

fn viewing_dir(spool: &Path) -> PathBuf {
    spool.with_file_name("viewing")
}

/// Tell tab `tab_id`'s status line which worker's view is on screen
/// (`Some`), or that the manager's own is again (`None`). Claude Code
/// reruns the status line every [`STATUSLINE_REFRESH_S`] seconds, so the
/// line follows within one.
pub fn set_viewing(spool: &Path, tab_id: &str, viewing: Option<&Viewing>) -> std::io::Result<()> {
    let flag = viewing_flag(spool, tab_id);
    match viewing {
        Some(viewing) => {
            if let Some(dir) = flag.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = flag.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(viewing)?)?;
            std::fs::rename(&tmp, &flag)
        }
        None => match std::fs::remove_file(&flag) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// Forget every tab's worker view: the app does at start, when no tab is on
/// one yet, so a run that ended on a worker's view leaves no line naming it.
pub fn clear_viewing(spool: &Path) {
    let _ = std::fs::remove_dir_all(viewing_dir(spool));
}

/// The worker whose view tab `tab_id` shows, as the app last told it
/// ([`set_viewing`]); `None` on the manager's own view. A flag from a
/// Giverny before the [`Viewing`] JSON held the name alone: it is read as
/// the label.
pub fn viewing_of(spool: &Path, tab_id: &str) -> Option<Viewing> {
    let text = std::fs::read_to_string(viewing_flag(spool, tab_id)).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(serde_json::from_str(text).unwrap_or_else(|_| Viewing {
        label: Some(text.to_string()),
        ..Viewing::default()
    }))
}

/// Where the app leaves its chrome's accent colour for the status line: beside
/// its socket, so a test instance's stays its own.
pub fn statusline_accent_path() -> PathBuf {
    socket_path().with_extension("accent")
}

/// Record the app's accent colour (`#rrggbb`) for the status line to draw a
/// session's name in; the app writes it at start and on every theme change.
pub fn write_statusline_accent(path: &Path, rgb: [u8; 3]) -> std::io::Result<()> {
    let [r, g, b] = rgb;
    let tmp = path.with_extension(format!("accent.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("#{r:02x}{g:02x}{b:02x}\n"))?;
    std::fs::rename(&tmp, path)
}

/// The SGR escape for the accent recorded at `path` (24-bit), else the
/// terminal's bright cyan, which is the default themes' accent (no Giverny
/// running, or one from before it recorded its accent).
fn accent_sgr(path: &Path) -> String {
    let rgb = std::fs::read_to_string(path).ok().and_then(|s| {
        let hex = s.trim().strip_prefix('#')?;
        let v = u32::from_str_radix(hex, 16)
            .ok()
            .filter(|_| hex.len() == 6)?;
        Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
    });
    match rgb {
        Some([r, g, b]) => format!("\x1b[38;2;{r};{g};{b}m"),
        None => "\x1b[96m".to_string(),
    }
}

/// `session: <n> (+<compacted>)`, `subagents: <n>` and `total: <n>` for the
/// status line (giverny#22, giverny#95): this conversation's own tokens, every subagent's
/// summed, and the two added, counted the way coo's `orchestrate-status`
/// counts them (see [`crate::tokens`]).
fn statusline_tokens(payload: &serde_json::Value, transcript: Option<&Path>) -> Vec<String> {
    use crate::tokens;
    let session_id = payload.get("session_id").and_then(|s| s.as_str());
    let dirs = tokens::session_subagent_dirs(transcript, config_dir().as_deref(), session_id);
    token_segments(payload, transcript, &tokens::subagent_transcripts(&dirs))
}

/// The token segments of one conversation ([`crate::tokens::segments`]):
/// its own count off `payload` or `transcript`, what it spent before its
/// compactions, and the `subagents` transcripts beside it. The main
/// session's line and a worker's ([`worker_statusline`]) both draw them.
fn token_segments(
    payload: &serde_json::Value,
    transcript: Option<&Path>,
    subagents: &[PathBuf],
) -> Vec<String> {
    use crate::tokens;
    let session = tokens::session_tokens(payload, transcript);
    // What the session spent before its compactions: `(+<n>)` beside its own
    // count, and in the total.
    let compacted = transcript.map_or(0, |t| {
        tokens::compacted_tokens_cached(t, tokens::compact_cache_dir().as_deref())
    });
    let (_, subagents, total) =
        tokens::session_subagents_total(session.map(|s| s.tokens), compacted, subagents);
    tokens::segments(session, compacted, subagents, total)
}

/// This account's Claude config dir.
fn config_dir() -> Option<PathBuf> {
    account_dir()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

/// The conversation's transcript: `transcript_path` off the status line's
/// stdin, else the one its session id names, as in coo.
fn transcript_of(payload: &serde_json::Value) -> Option<PathBuf> {
    let transcript = payload
        .get("transcript_path")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(PathBuf::from);
    transcript.filter(|t| t.exists()).or_else(|| {
        let sid = payload.get("session_id").and_then(|s| s.as_str())?;
        std::fs::read_dir(config_dir()?.join("projects"))
            .ok()?
            .flatten()
            .map(|e| e.path().join(format!("{sid}.jsonl")))
            .find(|p| p.is_file())
    })
}

// ---- subagentStatusLine: the management panel's live rows ----------------------

/// Synthetic event name for relayed `subagentStatusLine` input (not a Claude
/// hook event). The message's `event` is Claude Code's stdin verbatim —
/// `session_id`, `tasks[]`, … — with `hook_event_name` added, so
/// [`crate::subagents::LiveSnapshot::from_value`] reads it directly.
pub const SUBAGENT_LINE_EVENT: &str = "GivernySubagentLine";

/// The argument after `relay` that selects the subagent-line mode.
pub const SUBAGENT_LINE_FLAG: &str = "--subagent-line";

/// The `subagentStatusLine` command for one account's `settings.json`.
pub fn subagent_line_command_for(settings_path: &Path) -> String {
    format!("{} relay {SUBAGENT_LINE_FLAG}", exe_for(settings_path))
}

/// Is this `subagentStatusLine` command ours, whatever binary path it names?
fn is_our_subagent_line(command: &str) -> bool {
    command.contains("giverny") && command.trim_end().ends_with(SUBAGENT_LINE_FLAG)
}

/// How the relay has Claude Code draw its own subagent panel this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripRows {
    /// Every row as Claude Code draws it (the management panel is off).
    Native,
    /// No row at all: the management panel stands in for the panel.
    Hidden,
    /// Every row drawn, each opened by its agent id in brackets
    /// ([`tag_strip_row`]), for a walk to a worker's view.
    Tagged,
}

/// What the relay prints back to Claude Code for one tick: nothing for
/// [`StripRows::Native`], else one `{"id":…,"content":…}` line per task —
/// `""` for [`StripRows::Hidden`], the tagged label for
/// [`StripRows::Tagged`].
///
/// Claude Code 2.1.280 drops any row whose decoration is the empty string
/// from its subagent panel, and with every row dropped the panel — `● main`
/// included — is not drawn at all (verified in a live session).
/// Printing nothing leaves every row undecorated, which draws it natively.
/// A non-empty decoration takes the place of the row's type, label and
/// stats, after its pointer and dot (2.1.283).
pub fn subagent_line_output(payload: &serde_json::Value, rows: StripRows) -> String {
    if rows == StripRows::Native {
        return String::new();
    }
    let mut out = String::new();
    for task in payload
        .get("tasks")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        let Some(id) = task.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let content = match rows {
            StripRows::Tagged => {
                let text = |k: &str| task.get(k).and_then(|v| v.as_str()).map(str::trim);
                let label = text("label")
                    .filter(|l| !l.is_empty())
                    .or_else(|| text("description"))
                    .unwrap_or("");
                tag_strip_row(id, label)
            }
            _ => String::new(),
        };
        out.push_str(&serde_json::json!({ "id": id, "content": content }).to_string());
        out.push('\n');
    }
    out
}

/// A strip row's text while a walk looks for a worker:
/// `[<agent id>] <label>`. Claude Code labels a busy worker's row with a
/// model-written summary of what it is doing (`Reading runAgent.ts`),
/// which changes every thirty seconds and is never its description, so
/// the id is the one stable thing to find the row by.
pub fn tag_strip_row(id: &str, label: &str) -> String {
    if label.is_empty() {
        format!("[{id}]")
    } else {
        format!("[{id}] {label}")
    }
}

/// The agent id a [`tag_strip_row`] text opens with, and the rest.
pub fn strip_row_tag(text: &str) -> Option<(&str, &str)> {
    let (id, rest) = text.strip_prefix('[')?.split_once(']')?;
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok || !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some((id, rest.trim()))
}

/// The message the relay forwards for one `subagentStatusLine` tick.
pub fn subagent_line_msg(
    payload: serde_json::Value,
    target: Target,
    config_dir: Option<String>,
) -> RelayMsg {
    let mut event = match payload {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    event.insert(
        "hook_event_name".into(),
        serde_json::Value::String(SUBAGENT_LINE_EVENT.into()),
    );
    target.msg(config_dir, serde_json::Value::Object(event))
}

/// The `giverny relay --subagent-line` entrypoint: Claude Code runs it at
/// least every five seconds while a session has live workers, with the list
/// on stdin. Inside a Giverny tab it forwards that list to the app (the
/// management panel's Running rows) and, when `pane_on`, hides every row of Claude
/// Code's own panel — or, while the app has asked for the panel
/// ([`show_strip`]), draws each row tagged with its agent id. Outside a tab,
/// or in a session that is not the tab's own ([`crate::lineage`]), it does
/// nothing and prints nothing, so an account-wide install never changes a
/// session Giverny is not showing.
///
/// Like `relay`, it never fails: Claude Code logs a non-zero exit and drops
/// the tick, and a relay problem must not cost the user their panel.
pub fn run_subagent_line(spool: &Path, pane_on: bool) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1_000_000).read_to_string(&mut input);
    // Another session that inherited the tab's identity keeps its own panel
    // and its workers stay out of the tab's pane ([`crate::lineage`]).
    let Some(target) = Target::of_this_process() else {
        return;
    };
    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    // The strip is asked for per tab; a background job's tab is the app's
    // to know, so its panel stays hidden.
    let strip = match &target {
        Target::Tab(tab) => strip_wanted(spool, tab),
        Target::Job(_) => false,
    };
    let rows = match (pane_on, strip) {
        (false, _) => StripRows::Native,
        (true, false) => StripRows::Hidden,
        (true, true) => StripRows::Tagged,
    };
    let answer = subagent_line_output(&payload, rows);
    let account = target.account();
    deliver(&subagent_line_msg(payload, target, account), spool);
    print!("{answer}");
}

/// How long a [`show_strip`] request holds without being renewed. An app
/// that dies mid-request leaves the file behind; past this the relay hides
/// the panel again as if it were not there.
pub const STRIP_WANTED_FOR: Duration = Duration::from_secs(90);

/// The file that asks the relay to leave Claude Code's own subagent panel
/// drawn in tab `tab_id`, though the management panel is on: beside the spool,
/// one per tab.
pub fn strip_flag(spool: &Path, tab_id: &str) -> PathBuf {
    spool.with_file_name("show-strip").join(tab_id)
}

/// Ask the relay to draw Claude Code's subagent panel in `tab_id` (`on`),
/// or to go back to hiding it. The panel is what Claude Code's keyboard
/// path to a worker's view walks: hidden, `↓` has nothing to
/// reach. Claude Code runs the relay about every five seconds, so the
/// panel follows within one tick.
pub fn show_strip(spool: &Path, tab_id: &str, on: bool) -> std::io::Result<()> {
    let flag = strip_flag(spool, tab_id);
    if on {
        if let Some(dir) = flag.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&flag, b"")
    } else {
        match std::fs::remove_file(&flag) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Whether the app wants Claude Code's panel drawn in `tab_id` right now.
pub fn strip_wanted(spool: &Path, tab_id: &str) -> bool {
    std::fs::metadata(strip_flag(spool, tab_id))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < STRIP_WANTED_FOR)
}

/// Is the Giverny `subagentStatusLine` configured in this settings file?
pub fn subagent_line_installed_in(settings_path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    root.get("subagentStatusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(is_our_subagent_line)
}

/// Install or remove the Giverny `subagentStatusLine` in one settings file.
/// Returns whether the file changed.
///
/// The house rules for `settings.json`: a command the user configured
/// themselves is never replaced (an error, file untouched); nothing else in
/// the file moves; the first write leaves the same one-time backup
/// [`install_into`] does; and writing what is already there writes nothing,
/// so Claude Code's settings watcher is not woken for a no-op. Removing only
/// ever removes ours.
pub fn set_subagent_line(settings_path: &Path, enable: bool) -> anyhow::Result<bool> {
    let mut root: serde_json::Value = match std::fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("won't touch unparseable settings: {e}"))?,
        Err(_) if !enable => return Ok(false),
        Err(_) => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not an object"))?;
    let current = obj
        .get("subagentStatusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    if let Some(cmd) = &current
        && !is_our_subagent_line(cmd)
    {
        anyhow::bail!("a custom subagentStatusLine is already configured — leaving it alone");
    }
    let want = subagent_line_command_for(settings_path);
    match (enable, current.as_deref()) {
        (true, Some(cmd)) if cmd == want => return Ok(false),
        (false, None) => return Ok(false),
        _ => {}
    }
    let backup = settings_path.with_extension("json.giverny-bak");
    if settings_path.exists() && !backup.exists() {
        let _ = std::fs::copy(settings_path, &backup);
    }
    if enable {
        obj.insert(
            "subagentStatusLine".into(),
            serde_json::json!({ "type": "command", "command": want }),
        );
    } else {
        obj.remove("subagentStatusLine");
    }
    write_settings(settings_path, &root)?;
    Ok(true)
}

/// Is this statusLine command ours? Giverny only ever writes its binary
/// followed by ` statusline`. A command that just mentions giverny, like a
/// script of the user's under `~/giverny-tools/`, is theirs.
fn is_our_statusline(command: &str) -> bool {
    command.contains("giverny") && command.trim_end().ends_with(" statusline")
}

/// Is the Giverny statusline configured in this settings file?
pub fn statusline_installed_in(settings_path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    root.get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(is_our_statusline)
}

/// Does our statusline entry lack a `refreshInterval`? True only for an
/// entry of ours (installs from before it was written), never for a foreign
/// statusline or none at all; `set_statusline(.., true)` then adds it.
pub fn statusline_lacks_refresh(settings_path: &Path) -> bool {
    if !statusline_installed_in(settings_path) {
        return false;
    }
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    root.get("statusLine")
        .and_then(|s| s.get("refreshInterval"))
        .is_none_or(serde_json::Value::is_null)
}

/// Install/remove the Giverny statusline. Refuses to replace a statusline
/// the user configured themselves.
///
/// Enabling over an entry of ours (a path refresh) rewrites its `type`,
/// `command` and `refreshInterval` ([`STATUSLINE_REFRESH_S`]: the session's
/// use is pushed with each run, so the interval is the app's to set); a
/// `padding` or any other key the user set on it is kept.
pub fn set_statusline(settings_path: &Path, enable: bool) -> anyhow::Result<()> {
    let mut root: serde_json::Value = match std::fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(_) => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not an object"))?;
    let existing_is_foreign = obj
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(|c| !is_our_statusline(c));
    if existing_is_foreign {
        anyhow::bail!("a custom statusLine is already configured — leaving it alone");
    }
    if enable {
        let mut entry = match obj.remove("statusLine") {
            Some(serde_json::Value::Object(ours)) => ours,
            _ => serde_json::Map::new(),
        };
        entry.insert("type".into(), "command".into());
        entry.insert(
            "command".into(),
            statusline_command_for(settings_path).into(),
        );
        entry.entry("padding").or_insert(0.into());
        entry.insert("refreshInterval".into(), STATUSLINE_REFRESH_S.into());
        obj.insert("statusLine".into(), entry.into());
    } else {
        obj.remove("statusLine");
    }
    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(())
}

/// Start every Claude session in auto mode, via Claude Code's own
/// `permissions.defaultMode`.
///
/// Its own setting rather than a flag on the command line, because most
/// sessions are started by typing `claude`, not by Giverny. Verified against
/// 2.1.220's validator, which lists the accepted values as `acceptEdits`,
/// `auto`, `bypassPermissions`, `default`, `dontAsk`, `plan`.
///
/// Turning it off only removes a mode *we* set: a `defaultMode` the user
/// picked by hand is left alone, since silently reverting someone's
/// permission posture is the last thing this should do.
pub fn set_auto_mode(settings_path: &Path, enable: bool) -> anyhow::Result<()> {
    let mut root: serde_json::Value = match std::fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(_) => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not an object"))?;
    let current = obj
        .get("permissions")
        .and_then(|p| p.get("defaultMode"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    match (enable, current.as_deref()) {
        (true, Some("auto")) | (false, None) => return Ok(()),
        (false, Some(mode)) if mode != "auto" => {
            anyhow::bail!("permissions.defaultMode is set to {mode:?} — leaving it alone")
        }
        _ => {}
    }
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("permissions is not an object"))?;
    if enable {
        permissions.insert("defaultMode".into(), serde_json::json!("auto"));
    } else {
        permissions.remove("defaultMode");
    }
    // An empty block we created is noise in someone's config file.
    if permissions.is_empty() {
        obj.remove("permissions");
    }
    write_settings(settings_path, &root)
}

/// The permission mode this settings file starts sessions in, if it says.
pub fn default_mode_in(settings_path: &Path) -> Option<String> {
    let bytes = std::fs::read(settings_path).ok()?;
    let root: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(
        root.get("permissions")?
            .get("defaultMode")?
            .as_str()?
            .to_string(),
    )
}

/// Is auto mode the default in this settings file?
pub fn auto_mode_in(settings_path: &Path) -> bool {
    default_mode_in(settings_path).as_deref() == Some("auto")
}

fn write_settings(settings_path: &Path, root: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(())
}

fn is_our_entry(v: &serde_json::Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains("giverny") && c.trim_end().ends_with("relay"))
            })
        })
}

/// The relay command this settings file actually holds, whatever it is.
///
/// What a hook *says* it runs is the difference between a hook that works and
/// one that fires into nothing, and "installed ✓" cannot tell them apart.
pub fn installed_command(settings_path: &Path) -> Option<String> {
    let bytes = std::fs::read(settings_path).ok()?;
    let root: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    root.get("hooks")?
        .as_object()?
        .values()
        .filter_map(|v| v.as_array())
        .flatten()
        .filter(|entry| is_our_entry(entry))
        .find_map(|entry| {
            entry
                .get("hooks")?
                .as_array()?
                .iter()
                .find_map(|h| h.get("command")?.as_str().map(str::to_string))
        })
}

/// Is the relay present (for any exe path) in this settings file?
pub fn installed_in(settings_path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let Some(hooks) = root.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    RELAY_EVENTS.iter().all(|ev| {
        hooks
            .get(*ev)
            .and_then(|v| v.as_array())
            .is_some_and(|arr| arr.iter().any(is_our_entry))
    })
}

/// Do this profile's Giverny entries point at a *different* binary than the
/// one running now? Happens after `cargo install`, a rebuild elsewhere, or
/// moving the binary — the hooks then silently invoke a path that may no
/// longer exist.
pub fn needs_path_refresh(settings_path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let want_relay = relay_command_for(settings_path);
    let hooks_stale = root
        .get("hooks")
        .and_then(|h| h.as_object())
        .is_some_and(|hooks| {
            hooks.values().any(|v| {
                v.as_array().is_some_and(|arr| {
                    arr.iter().filter(|e| is_our_entry(e)).any(|e| {
                        e.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
                            hs.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .is_some_and(|c| c != want_relay)
                            })
                        })
                    })
                })
            })
        });
    let want_statusline = statusline_command_for(settings_path);
    let ours = root.get("statusLine").filter(|s| {
        s.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(is_our_statusline)
    });
    // Ours, but another binary's, or written before it reran every second.
    let statusline_stale = ours.is_some_and(|s| {
        s.get("command").and_then(|c| c.as_str()) != Some(want_statusline.as_str())
            || s.get("refreshInterval").and_then(|r| r.as_u64()) != Some(STATUSLINE_REFRESH_S)
    });
    let want_subagent_line = subagent_line_command_for(settings_path);
    let subagent_line_stale = root
        .get("subagentStatusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(|c| is_our_subagent_line(c) && c != want_subagent_line);
    hooks_stale || statusline_stale || subagent_line_stale
}

/// Install (or refresh) the relay hooks in one profile's `settings.json`.
/// Non-destructive: existing hooks are preserved; our stale entries (old exe
/// paths) are replaced. A one-time backup lands beside the file.
pub fn install_into(settings_path: &Path) -> anyhow::Result<bool> {
    let mut root: serde_json::Value = match std::fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("won't touch unparseable settings: {e}"))?,
        Err(_) => serde_json::json!({}),
    };
    if !root.is_object() {
        anyhow::bail!("settings root is not an object");
    }

    let backup = settings_path.with_extension("json.giverny-bak");
    if settings_path.exists() && !backup.exists() {
        let _ = std::fs::copy(settings_path, &backup);
    }

    let command = relay_command_for(settings_path);
    let mut changed = false;
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        anyhow::bail!("settings.hooks is not an object");
    }
    for ev in RELAY_EVENTS {
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*ev)
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = arr.as_array_mut() else {
            anyhow::bail!("settings.hooks.{ev} is not an array");
        };
        // Drop stale giverny entries (old binary paths), then append current.
        let had = list.len();
        list.retain(|entry| !is_our_entry(entry));
        let ours = serde_json::json!({
            "hooks": [{ "type": "command", "command": command, "async": true, "timeout": 10 }]
        });
        let already = had == list.len() + 1 && {
            // We removed exactly one of ours — was it identical?
            false
        };
        list.push(ours);
        changed |= !already || had != list.len();
    }

    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(changed)
}

/// Remove our relay entries from one settings file.
///
/// The management panel's `subagentStatusLine` goes with them when it is ours: it
/// is `relay --subagent-line`, the same relay, and a relay left behind after
/// an uninstall runs a binary nobody asked to keep. One the user configured
/// themselves is never touched.
pub fn uninstall_from(settings_path: &Path) -> anyhow::Result<()> {
    // The `giverny` plugin's keys, where they are ours.
    let _ = crate::plugin::set_plugin(settings_path, Path::new(""), false);
    let Ok(bytes) = std::fs::read(settings_path) else {
        return Ok(());
    };
    let mut root: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_, v) in hooks.iter_mut() {
            if let Some(list) = v.as_array_mut() {
                list.retain(|entry| !is_our_entry(entry));
            }
        }
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }
    if let Some(obj) = root.as_object_mut()
        && obj
            .get("subagentStatusLine")
            .and_then(|s| s.get("command"))
            .and_then(|c| c.as_str())
            .is_some_and(is_our_subagent_line)
    {
        obj.remove("subagentStatusLine");
    }
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short socket path of its own: a long one does not fit `sun_path`.
    #[cfg(unix)]
    fn scratch_socket(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gv-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[cfg(unix)]
    #[test]
    fn only_the_raise_request_reads_as_one() {
        assert!(is_raise_request(RAISE_REQUEST));
        assert!(!is_raise_request(r#"{"giverny":"other"}"#));
        assert!(!is_raise_request(
            r#"{"tab_id":"1","config_dir":null,"event":{"hook_event_name":"Stop"}}"#
        ));
        assert!(!is_raise_request("not json"));
    }

    #[cfg(unix)]
    #[test]
    fn nobody_on_the_socket_means_this_launch_opens_the_window() {
        let path = scratch_socket("none");
        assert_eq!(
            ask_to_raise(&path, Duration::from_millis(200)),
            Running::Nobody
        );
        assert_eq!(socket_holder(&path), None);
        // A socket file left by a Giverny that is gone refuses connections.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert_eq!(
            ask_to_raise(&path, Duration::from_millis(200)),
            Running::Nobody
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_running_giverny_answers_and_a_hung_one_is_named() {
        use std::io::BufRead;
        let path = scratch_socket("raise");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let ours = cfg!(target_os = "linux").then_some(std::process::id());
        assert_eq!(socket_holder(&path), Some(Holder { pid: ours }));
        // `socket_holder`'s own connection is still queued: take it.
        drop(listener.accept().unwrap());

        // Alive: reads the request, answers it.
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap();
            assert!(is_raise_request(line.trim()));
            (&stream).write_all(raise_reply().as_bytes()).unwrap();
            listener
        });
        assert_eq!(
            ask_to_raise(&path, Duration::from_secs(5)),
            Running::Raised {
                pid: Some(std::process::id())
            }
        );
        let listener = server.join().unwrap();

        // Hung: the connection is accepted (by the kernel) and never answered.
        assert_eq!(
            ask_to_raise(&path, Duration::from_millis(200)),
            Running::Silent(Holder { pid: ours })
        );
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_held_relay_names_the_pid_that_holds_it() {
        let held = Held {
            path: PathBuf::from("/run/user/1000/giverny.sock"),
            holder: Holder { pid: Some(408784) },
        };
        assert_eq!(
            held.to_string(),
            "another Giverny (pid 408784) holds the relay at /run/user/1000/giverny.sock \
             — this window will not receive hook events"
        );
    }

    #[test]
    fn a_cold_prompt_cache_shows_red_with_what_the_next_message_recaches() {
        let cold = |extra: serde_json::Value| {
            let mut cache = serde_json::json!({"warm": false, "caching_observed": true});
            cache
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            cache_cold_segment(&serde_json::json!({ "prompt_cache": cache }), None, 0)
        };
        assert_eq!(
            cold(serde_json::json!({"recache_tokens_if_cold": 182_340})).as_deref(),
            Some("\x1b[31mcache cold · next msg 182.3k\x1b[0m")
        );
        // Right after a compaction there is no figure.
        assert_eq!(
            cold(serde_json::json!({"recache_tokens_if_cold": null})).as_deref(),
            Some("\x1b[31mcache cold\x1b[0m")
        );
        // Warm, unreported caching, or an older Claude Code: nothing.
        assert_eq!(cold(serde_json::json!({"warm": true})), None);
        assert_eq!(cold(serde_json::json!({"caching_observed": false})), None);
        assert_eq!(cache_cold_segment(&serde_json::json!({}), None, 0), None);
    }

    #[test]
    fn a_reopened_session_reads_its_cold_cache_off_the_transcript() {
        let d = std::env::temp_dir().join(format!("giverny-cache-cold-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let t = d.join("s.jsonl");
        let line = serde_json::json!({"type": "assistant", "timestamp": "2026-10-06T11:00:00Z",
            "message": {"role": "assistant", "model": "claude-opus-5-5", "content": [],
                "usage": {"input_tokens": 2, "cache_creation_input_tokens": 1000,
                    "cache_read_input_tokens": 90_000, "output_tokens": 500,
                    "cache_creation": {"ephemeral_1h_input_tokens": 1000}}}});
        std::fs::write(&t, format!("{line}\n")).unwrap();
        let at: i64 = "2026-10-06T11:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        // No `prompt_cache` on stdin before the reopened session's first request.
        let none = serde_json::json!({});
        assert_eq!(cache_cold_segment(&none, Some(&t), at + 3_599_000), None);
        assert_eq!(
            cache_cold_segment(&none, Some(&t), at + 3_600_000).as_deref(),
            Some("\x1b[31mcache cold · next msg 91.5k\x1b[0m")
        );
        // Once Claude Code reports the cache, its word wins.
        let warm = serde_json::json!({"prompt_cache": {"warm": true, "caching_observed": true}});
        assert_eq!(cache_cold_segment(&warm, Some(&t), at + 7_200_000), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Off Windows — and for a Windows account that is not inside a
    /// distribution — the command is this binary, named as it always was.
    #[test]
    fn the_hook_command_names_this_binary() {
        let settings = std::env::temp_dir().join("giverny-hookcmd/settings.json");
        assert_eq!(relay_command_for(&settings), relay_command());
        assert!(relay_command_for(&settings).trim_end().ends_with("relay"));
        assert!(
            statusline_command_for(&settings)
                .trim_end()
                .ends_with("statusline")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_link_follows_the_binary() {
        let d = std::env::temp_dir().join(format!("giverny-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // The binary is named by its real path (macOS's /tmp is a link).
        let d = d.canonicalize().unwrap();
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let link = d.join("bin/giverny");
        assert!(!is_link_to_a_binary(&link), "not there yet");
        point_link_at(&link, &a).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), a);
        assert!(is_link_to_a_binary(&link));
        point_link_at(&link, &a).unwrap();
        point_link_at(&link, &b).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), b, "repointed");
        std::fs::remove_file(&b).unwrap();
        assert!(
            !is_link_to_a_binary(&link),
            "a link to nothing is no binary"
        );
        assert!(is_dangling(&link), "and is taken back");
        point_link_at(&link, &a).unwrap();
        assert!(!is_dangling(&link));
        assert!(
            !is_dangling(&d.join("nothing")),
            "no link is not a dangling one"
        );

        // Started through the link, the binary is still the one it names,
        // never the link itself.
        point_link_at(&link, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), a);
        assert!(is_link_to_a_binary(&link));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// macOS's config dir is `~/Library/Application Support`: the hook
    /// command must still run the binary there, through the shell Claude
    /// Code runs it with.
    #[cfg(unix)]
    #[test]
    fn a_path_with_a_space_runs_as_one_command() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("giverny-quote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let dir = d.join("Application Support/giverny's bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("giverny");
        std::fs::write(&exe, "#!/bin/sh\necho \"ran $1\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let command = format!("{} relay", command_word(&exe));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "ran relay\n",
            "{command}"
        );
        assert_eq!(
            command_word(Path::new("/home/x/.config/giverny/bin/giverny")),
            "/home/x/.config/giverny/bin/giverny",
            "a plain path is written as before"
        );
        // Windows: forward slashes, which Git Bash and PowerShell both run.
        assert_eq!(
            word_for_shell(r"C:\Users\ita\AppData\Local\Giverny\bin\giverny.exe", true),
            "C:/Users/ita/AppData/Local/Giverny/bin/giverny.exe"
        );
        assert_eq!(
            word_for_shell(r"C:\Users\Jane Doe\giverny.exe", true),
            "'C:/Users/Jane Doe/giverny.exe'"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn auto_mode_is_written_and_taken_back() {
        let settings = scratch("automode");
        std::fs::write(
            &settings,
            br#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .unwrap();

        set_auto_mode(&settings, true).unwrap();
        assert!(auto_mode_in(&settings));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "the rest of the file survives");
        assert_eq!(
            root["permissions"]["allow"][0], "Bash(ls:*)",
            "existing permission rules survive"
        );

        set_auto_mode(&settings, false).unwrap();
        assert!(!auto_mode_in(&settings));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert!(root["permissions"]["defaultMode"].is_null());
        assert_eq!(root["permissions"]["allow"][0], "Bash(ls:*)");
    }

    #[test]
    fn a_mode_set_by_hand_is_left_alone() {
        let settings = scratch("automode-manual");
        std::fs::write(&settings, br#"{"permissions":{"defaultMode":"plan"}}"#).unwrap();
        // Turning the toggle off must not revert someone else's choice.
        assert!(set_auto_mode(&settings, false).is_err());
        assert_eq!(default_mode_in(&settings).as_deref(), Some("plan"));
    }

    #[test]
    fn auto_mode_creates_a_settings_file_that_did_not_exist() {
        let settings = scratch("automode-new");
        std::fs::remove_file(&settings).ok();
        set_auto_mode(&settings, true).unwrap();
        assert!(auto_mode_in(&settings));
        // ...and turning it off leaves no empty scaffolding behind.
        set_auto_mode(&settings, false).unwrap();
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert!(root.get("permissions").is_none(), "no empty block left");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn install_preserves_existing_hooks() {
        let path = scratch("preserve");
        std::fs::write(
            &path,
            r#"{ "effortLevel": "xhigh",
                 "hooks": { "Notification": [ { "hooks": [
                   { "type": "command", "command": "~/.claude/hooks/notify.sh", "async": true } ] } ] } }"#,
        )
        .unwrap();
        install_into(&path).unwrap();
        assert!(installed_in(&path));

        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(root["effortLevel"], "xhigh", "unrelated settings preserved");
        let notif = root["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(
            notif.len(),
            2,
            "user's notify.sh entry survives next to ours"
        );
        assert!(
            path.with_extension("json.giverny-bak").exists(),
            "backup created"
        );
    }

    #[test]
    fn install_is_idempotent_and_refreshes_path() {
        let path = scratch("idem");
        install_into(&path).unwrap();
        install_into(&path).unwrap();
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for ev in RELAY_EVENTS {
            let arr = root["hooks"][ev].as_array().unwrap();
            assert_eq!(
                arr.len(),
                1,
                "{ev}: exactly one giverny entry after reinstall"
            );
        }
        assert!(installed_in(&path));
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let path = scratch("uninstall");
        std::fs::write(
            &path,
            r#"{ "hooks": { "Stop": [ { "hooks": [
                 { "type": "command", "command": "echo mine" } ] } ] } }"#,
        )
        .unwrap();
        install_into(&path).unwrap();
        uninstall_from(&path).unwrap();
        assert!(!installed_in(&path));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "user's entry kept");
        assert_eq!(stop[0]["hooks"][0]["command"], "echo mine");
    }

    #[test]
    fn detects_stale_binary_paths() {
        let path = scratch("stale-path");
        install_into(&path).unwrap();
        set_statusline(&path, true).unwrap();
        assert!(
            !needs_path_refresh(&path),
            "freshly installed entries match the running binary"
        );

        // Simulate the entries having been written by a binary living
        // somewhere else (what `cargo install` or a moved build produces).
        let text = std::fs::read_to_string(&path).unwrap();
        let stale = text.replace(&relay_command(), "/old/path/giverny relay");
        std::fs::write(&path, stale).unwrap();
        assert!(
            needs_path_refresh(&path),
            "a different exe path is detected"
        );

        install_into(&path).unwrap();
        assert!(!needs_path_refresh(&path), "reinstalling repairs the path");
        assert!(installed_in(&path));
    }

    #[test]
    fn spool_watcher_delivers_and_clears() {
        let dir = std::env::temp_dir().join(format!("giverny-spool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spool = dir.join("hook-spool.jsonl");
        let line = r#"{"tab_id":"giverny-3","config_dir":null,"event":{"hook_event_name":"Stop"}}"#;
        std::fs::write(&spool, format!("{line}\n")).unwrap();

        // Messages already on disk come back immediately...
        let (rx, spooled) = spawn_spool_watcher(&spool, || {}).unwrap();
        assert_eq!(spooled.len(), 1);
        assert!(!spool.exists(), "spool is consumed, not replayed forever");

        // ...and later appends arrive through the channel.
        std::fs::write(&spool, format!("{line}\n")).unwrap();
        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("watcher delivers appended lines");
        assert_eq!(msg.hook_event(), Some("Stop"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A statusline of the user's whose path only mentions giverny is still
    /// theirs: not counted as ours, not stale, and never replaced.
    #[test]
    fn a_user_statusline_under_a_giverny_path_is_theirs() {
        let path = scratch("statusline-theirs");
        for command in ["~/giverny-tools/line.sh", "~/bin/giverny-statusline"] {
            let theirs = format!(r#"{{"statusLine":{{"type":"command","command":"{command}"}}}}"#);
            std::fs::write(&path, &theirs).unwrap();
            assert!(!statusline_installed_in(&path), "{command}");
            assert!(!needs_path_refresh(&path), "{command}");
            assert!(set_statusline(&path, true).is_err(), "{command}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
        }
    }

    #[test]
    fn statusline_install_and_respect_existing() {
        let path = scratch("statusline");
        set_statusline(&path, true).unwrap();
        assert!(statusline_installed_in(&path));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            root["statusLine"]["refreshInterval"], 1,
            "reruns every second"
        );
        assert!(!needs_path_refresh(&path));
        // Ours from before it reran every second: brought up to date.
        let mut old = root.clone();
        old["statusLine"]
            .as_object_mut()
            .unwrap()
            .remove("refreshInterval");
        std::fs::write(&path, old.to_string()).unwrap();
        assert!(needs_path_refresh(&path));
        set_statusline(&path, false).unwrap();
        assert!(!statusline_installed_in(&path));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(root.get("statusLine").is_none(), "goes with ours");

        std::fs::write(
            &path,
            r#"{"statusLine":{"type":"command","command":"my-own-script.sh"}}"#,
        )
        .unwrap();
        assert!(
            set_statusline(&path, true).is_err(),
            "must not clobber a user statusline"
        );
        assert!(!needs_path_refresh(&path), "nor bring a user's up to date");
    }

    #[test]
    fn subagent_line_never_replaces_the_users_own() {
        let path = scratch("subline-foreign");
        let theirs = r#"{"subagentStatusLine":{"type":"command","command":"~/bin/agents.sh"}}"#;
        std::fs::write(&path, theirs).unwrap();
        assert!(set_subagent_line(&path, true).is_err(), "on: refused");
        assert!(
            set_subagent_line(&path, false).is_err(),
            "off: not ours to remove"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs, "untouched");
        assert!(!subagent_line_installed_in(&path));
    }

    /// Uninstalling takes the management panel's line with the hooks — it is the
    /// same relay — and leaves a user's own line where it was.
    #[test]
    fn uninstall_takes_our_subagent_line_and_leaves_theirs() {
        let path = scratch("subline-uninstall");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        install_into(&path).unwrap();
        set_subagent_line(&path, true).unwrap();
        uninstall_from(&path).unwrap();
        assert!(!installed_in(&path));
        assert!(!subagent_line_installed_in(&path));
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(root.get("subagentStatusLine").is_none());
        assert_eq!(root["model"], "opus", "the rest of the file survives");

        let theirs = scratch("subline-uninstall-theirs");
        std::fs::write(
            &theirs,
            r#"{"subagentStatusLine":{"type":"command","command":"~/bin/agents.sh"}}"#,
        )
        .unwrap();
        install_into(&theirs).unwrap();
        uninstall_from(&theirs).unwrap();
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&theirs).unwrap()).unwrap();
        assert_eq!(root["subagentStatusLine"]["command"], "~/bin/agents.sh");
    }

    fn statusline_entry(path: &Path) -> serde_json::Value {
        let root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        root["statusLine"].clone()
    }

    #[test]
    fn a_fresh_statusline_refreshes_every_second() {
        let path = scratch("statusline-refresh-fresh");
        set_statusline(&path, true).unwrap();
        let entry = statusline_entry(&path);
        assert_eq!(entry["refreshInterval"], STATUSLINE_REFRESH_S);
        assert_eq!(entry["refreshInterval"], 1);
        assert_eq!(entry["padding"], 0);
        assert!(!statusline_lacks_refresh(&path));
    }

    #[test]
    fn a_path_refresh_keeps_the_users_keys_but_sets_the_interval() {
        let path = scratch("statusline-refresh-user");
        std::fs::write(
            &path,
            r#"{"statusLine":{"type":"command","command":"/old/path/giverny statusline",
                "padding":2,"refreshInterval":5,"note":"mine"}}"#,
        )
        .unwrap();
        assert!(needs_path_refresh(&path));
        assert!(!statusline_lacks_refresh(&path), "it has one");

        set_statusline(&path, true).unwrap();
        let entry = statusline_entry(&path);
        assert_eq!(entry["command"], statusline_command_for(&path));
        // The use pushed with each run keeps step with the app's sampler
        // only at the app's interval.
        assert_eq!(entry["refreshInterval"], STATUSLINE_REFRESH_S);
        assert_eq!(entry["padding"], 2);
        assert_eq!(entry["note"], "mine", "other keys of the user's are kept");
        assert!(!needs_path_refresh(&path));
    }

    #[test]
    fn an_older_entry_of_ours_gains_the_refresh_interval() {
        let path = scratch("statusline-refresh-old");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "statusLine": {
                    "type": "command",
                    "command": statusline_command_for(&path),
                    "padding": 0,
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(statusline_installed_in(&path));
        // The path is current, but without the app's interval the entry is
        // stale all the same.
        assert!(needs_path_refresh(&path));
        assert!(statusline_lacks_refresh(&path));

        set_statusline(&path, true).unwrap();
        assert_eq!(
            statusline_entry(&path)["refreshInterval"],
            STATUSLINE_REFRESH_S
        );
        assert!(!statusline_lacks_refresh(&path));
    }

    #[test]
    fn a_foreign_statusline_never_lacks_a_refresh_interval() {
        let path = scratch("statusline-refresh-foreign");
        let mine = r#"{"statusLine":{"type":"command","command":"my-own-script.sh"}}"#;
        std::fs::write(&path, mine).unwrap();
        assert!(!statusline_lacks_refresh(&path), "not ours to change");
        assert!(set_statusline(&path, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);

        let none = scratch("statusline-refresh-none");
        assert!(!statusline_lacks_refresh(&none), "no file, nothing to add");
    }

    #[test]
    fn relay_msg_accessors() {
        let msg: RelayMsg = serde_json::from_str(
            r#"{"tab_id":"giverny-3","config_dir":"/home/u/.claude",
                "event":{"hook_event_name":"Notification","session_id":"s1",
                         "notification_type":"permission_prompt","message":"needs ok"}}"#,
        )
        .unwrap();
        assert_eq!(msg.hook_event(), Some("Notification"));
        assert_eq!(msg.session_id(), Some("s1"));
        assert_eq!(msg.notification_type(), Some("permission_prompt"));
        assert_eq!(msg.message(), Some("needs ok"));
    }

    /// A `Stop` whose session left subagents running says so in its
    /// `background_tasks`; a background shell or a monitor alone is not
    /// delegated work, and a finished entry or an older Claude Code that
    /// sends no list is none.
    #[test]
    fn a_stop_says_whether_delegated_work_still_runs() {
        let stop = |tasks: &str| -> RelayMsg {
            serde_json::from_str(&format!(
                r#"{{"tab_id":"giverny-3","config_dir":null,
                    "event":{{"hook_event_name":"Stop","session_id":"s1"{tasks}}}}}"#
            ))
            .unwrap()
        };
        let agent = r#","background_tasks":[{"id":"a1","type":"subagent","status":"running",
            "description":"23: held orders","agent_type":"general-purpose"}]"#;
        assert!(stop(agent).delegated_work_running());
        let raw = r#","background_tasks":[{"id":"w1","type":"local_workflow","status":"pending"}]"#;
        assert!(stop(raw).delegated_work_running(), "a raw type counts too");
        let shell = r#","background_tasks":[{"id":"b1","type":"shell","status":"running",
            "command":"sleep 99"},{"id":"m1","type":"monitor","status":"running"}]"#;
        assert!(!stop(shell).delegated_work_running());
        let done = r#","background_tasks":[{"id":"a1","type":"subagent","status":"completed"}]"#;
        assert!(!stop(done).delegated_work_running());
        assert!(!stop(r#","background_tasks":[]"#).delegated_work_running());
        assert!(
            !stop("").delegated_work_running(),
            "no list, as before 2.1.296"
        );
    }
}
