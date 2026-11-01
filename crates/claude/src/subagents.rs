//! Subagents: the workers a tab's Claude session has spawned, as rows.
//!
//! The management panel (off by default, `claude.management_panel`) lists the subagents
//! of the Claude session in the tab you are looking at — the ones running and
//! the ones that finished — so a finished worker stays on screen until
//! `/clear` instead of vanishing thirty seconds after it lands. This module
//! is the model behind that list, and it needs no setup from the user: every
//! row comes from data Claude Code already writes.
//!
//! Three sources, each carrying what the others cannot:
//!
//! * **The live list.** Claude Code hands its `subagentStatusLine` command a
//!   JSON object on stdin at least every five seconds while workers run:
//!   `session_id`, and `tasks[]` with `id`, `type`, `status`, `description`,
//!   `label`, `startTime`, `model`, `tokenCount`, `cwd`, and `name` only when
//!   the agent was given one. [`LiveSnapshot::parse`] reads it. It is the
//!   authority on what is running *now*.
//! * **The worker's own transcript.**
//!   `projects/<munged cwd>/<parent session>/subagents/agent-<id>.jsonl`, with
//!   `agent-<id>.meta.json` beside it (`description`, `agentType`, `model`,
//!   and for an agent another agent spawned `parentAgentId` and
//!   `spawnDepth`: what nests it under its parent's row).
//!   The last `tool_use` in it is what the worker is doing ([`read_activity`]).
//! * **The parent's transcript.** When a worker stops, Claude Code queues a
//!   `<task-notification>` naming its `<task-id>` and a `<status>` —
//!   `completed`, `failed`, `killed` or `stopped` on every transcript on the
//!   machine this was written against. [`NotificationTail`] follows the parent
//!   transcript and collects them. A worker that is messaged again after it
//!   finished writes new lines to its transcript and gets a *new*
//!   notification when it stops again, so "done" is always "the latest
//!   notification is newer than the worker's latest line", never "a
//!   notification exists".
//!
//! [`Tracker`] folds the three into [`SubagentRow`]s for one tab. It is
//! serde-serialisable so the tab can persist it and a Giverny restart keeps
//! the Done rows.
//!
//! **Merging with the feed.** The optional feed (`feed.rs`) adds Planned
//! rows, titles, ETAs and landings. A feed row carrying `agent_id` merges with
//! the [`SubagentRow`] whose [`SubagentRow::id`] equals it — that string, the
//! agent id in `agent-<id>.jsonl` and in `tasks[].id`, is the one join key
//! and is never rewritten. The native row owns [`SubagentRow::started_ms`],
//! [`SubagentRow::tokens`] and [`SubagentRow::activity`]; the feed owns
//! title, ETA and landing. [`Tracker::get`] is the lookup the merge uses.
//!
//! Everything read here is Claude Code's private state and will change shape.
//! Fields are pulled out of `serde_json::Value` one at a time, as in
//! `jobs.rs`, so a surprise costs one field and never the row.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Field helpers (tolerant: a wrong type is an absent field)
// ---------------------------------------------------------------------------

fn as_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(Into::into)
}

fn as_u64(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        _ => None,
    }
}

/// Milliseconds since the epoch, from a number or an RFC 3339 string.
fn as_millis(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_rfc3339_ms(s),
        _ => None,
    }
}

fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond().max(0) as u64)
}

// ---------------------------------------------------------------------------
// Stage and outcome
// ---------------------------------------------------------------------------

/// Where a native row stands. Planned rows exist only in the feed, so they
/// are not a stage here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Running,
    Done,
}

/// How a Done row ended, from the `<status>` of its notification or the
/// `status` of its live-list entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Completed,
    Failed,
    /// Killed by the user or the parent (`TaskStop`).
    Killed,
    Stopped,
    /// It left the live list and no notification has said how. The usual
    /// case for the few seconds between the two, and the permanent one when
    /// the parent transcript cannot be found.
    Unknown,
}

impl Outcome {
    /// Map a status word. `None` for the words that mean "still going".
    pub fn parse(s: &str) -> Option<Outcome> {
        match s {
            "running" | "pending" | "queued" | "starting" | "" => None,
            "completed" | "complete" | "done" | "finished" | "success" => Some(Outcome::Completed),
            "failed" | "error" | "errored" => Some(Outcome::Failed),
            "killed" | "cancelled" | "canceled" => Some(Outcome::Killed),
            "stopped" => Some(Outcome::Stopped),
            _ => Some(Outcome::Unknown),
        }
    }

    /// Anything but a clean completion — worth a warning colour.
    pub fn is_bad(self) -> bool {
        matches!(self, Outcome::Failed | Outcome::Killed)
    }
}

// ---------------------------------------------------------------------------
// The live list (subagentStatusLine stdin)
// ---------------------------------------------------------------------------

/// One `tasks[]` entry of the `subagentStatusLine` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTask {
    /// The agent id — `agent-<id>.jsonl`, and the merge key.
    pub id: String,
    /// `local_agent` for a subagent; carried through as-is.
    pub kind: Option<String>,
    /// Raw status word (`running`, `completed`, …).
    pub status: String,
    /// The Agent tool's `description`.
    pub description: Option<String>,
    /// Claude Code's own one-line "what it's doing" (often empty).
    pub label: Option<String>,
    /// Present only for an agent that was given a name.
    pub name: Option<String>,
    pub start_ms: Option<u64>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<PathBuf>,
}

impl LiveTask {
    fn from_value(v: &Value) -> Option<LiveTask> {
        Some(LiveTask {
            id: as_str(v, "id")?,
            kind: as_str(v, "type"),
            status: as_str(v, "status").unwrap_or_default(),
            description: as_str(v, "description"),
            label: as_str(v, "label"),
            name: as_str(v, "name"),
            start_ms: as_millis(v, "startTime"),
            model: as_str(v, "model"),
            tokens: as_u64(v, "tokenCount"),
            cwd: as_str(v, "cwd").map(PathBuf::from),
        })
    }

    /// Still going, by its own status word.
    pub fn is_running(&self) -> bool {
        Outcome::parse(&self.status).is_none()
    }
}

/// One `subagentStatusLine` input: whose session, and its visible workers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveSnapshot {
    pub session_id: Option<String>,
    pub tasks: Vec<LiveTask>,
}

impl LiveSnapshot {
    /// Parse the stdin JSON. Never fails: garbage is an empty snapshot, an
    /// entry with no `id` is skipped.
    pub fn parse(json: &str) -> LiveSnapshot {
        serde_json::from_str::<Value>(json)
            .map(|v| LiveSnapshot::from_value(&v))
            .unwrap_or_default()
    }

    pub fn from_value(v: &Value) -> LiveSnapshot {
        LiveSnapshot {
            session_id: as_str(v, "session_id"),
            tasks: v
                .get("tasks")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(LiveTask::from_value).collect())
                .unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Files on disk
// ---------------------------------------------------------------------------

/// The `subagents/` directory of a parent session:
/// `projects/<munged cwd>/<session>/subagents`. Found by scanning project
/// dirs, since the munging is lossy (as `registry::find_transcript`).
pub fn subagents_dir(config_dir: &Path, parent_session: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(config_dir.join("projects"))
        .ok()?
        .flatten()
    {
        let candidate = entry.path().join(parent_session).join("subagents");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// A parent session's own transcript: `projects/<munged cwd>/<session>.jsonl`,
/// found by scanning project dirs as [`subagents_dir`] does.
pub fn session_transcript(config_dir: &Path, session: &str) -> Option<PathBuf> {
    std::fs::read_dir(config_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(format!("{session}.jsonl")))
        .find(|p| p.is_file())
}

/// Lines of a transcript's head the conversation's root can hide in.
const ROOT_SCAN: usize = 500;

/// The conversation a transcript holds: the `uuid` of its first parentless
/// user or assistant turn. Claude Code copies a conversation's records
/// forward when it re-ids a session, so this is the same before and after
/// where the session id is not; `/clear` starts a new root, and a different
/// conversation has its own (a key a manager can file its sessions by).
/// `None` for a transcript with no turn of its own yet — a session
/// just switched to in the agents view holds only headers until its next
/// turn.
pub fn conversation_root(transcript: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(transcript).ok()?;
    for line in std::io::BufReader::new(file).lines().take(ROOT_SCAN) {
        let Ok(line) = line else { break };
        if !line.contains("\"uuid\"") || !line.contains("\"parentUuid\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if !matches!(
            v.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            continue;
        }
        let parent = v.get("parentUuid").and_then(Value::as_str);
        if parent.is_none_or(str::is_empty)
            && let Some(uuid) = as_str(&v, "uuid")
        {
            return Some(uuid);
        }
    }
    None
}

/// `agent-<id>.jsonl` inside a `subagents/` dir.
pub fn agent_transcript(subagents_dir: &Path, id: &str) -> PathBuf {
    subagents_dir.join(format!("agent-{id}.jsonl"))
}

/// Agent ids with a transcript in a `subagents/` dir.
pub fn list_agent_ids(subagents_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(subagents_dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("agent-")?
                .strip_suffix(".jsonl")
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

/// `agent-<id>.meta.json`: what the spawn said about the worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMeta {
    pub description: Option<String>,
    pub agent_type: Option<String>,
    /// Only when the spawn passed one.
    pub model: Option<String>,
    /// The agent that spawned it (`parentAgentId`), when that was an agent
    /// and not the session itself.
    pub parent_id: Option<String>,
    /// How deep it was spawned (`spawnDepth`): 1 for the session's own
    /// workers, 2 for an agent one of them spawned, and so on.
    pub depth: Option<u32>,
}

pub fn read_meta(subagents_dir: &Path, id: &str) -> AgentMeta {
    let path = subagents_dir.join(format!("agent-{id}.meta.json"));
    let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return AgentMeta::default();
    };
    AgentMeta {
        description: as_str(&v, "description"),
        agent_type: as_str(&v, "agentType"),
        model: as_str(&v, "model"),
        parent_id: as_str(&v, "parentAgentId"),
        depth: as_u64(&v, "spawnDepth").map(|d| d.min(u64::from(u32::MAX)) as u32),
    }
}

/// What a worker's transcript says about it, read from its tail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// The last tool call, one line: `Bash: cargo test`, `Read: src/lib.rs`.
    pub doing: Option<String>,
    /// Timestamp of the newest *assistant* line — the worker's last turn.
    /// Only its own turns count: a background shell it left running can
    /// land a notification line in its transcript after it has finished,
    /// and that is not the worker working (seen on real transcripts).
    pub last_turn_ms: Option<u64>,
    /// The context the worker carries now: input + cache creation + cache
    /// read of its *last* API response, assigned, never summed — the rule
    /// [`crate::tokens`] counts by, so the pane and the status line
    /// agree.
    pub tokens: Option<u64>,
    /// The model the last assistant turn ran on.
    pub model: Option<String>,
    /// Spans the worker stood stopped on an API error, oldest first: from
    /// Claude Code's `<synthetic>` `isApiErrorMessage` line to its next real
    /// turn, the last one open while nothing came after it.
    pub stops: Vec<Stop>,
}

/// A span a worker stood stopped on an API error — no network, a usage
/// limit, an expired login — and its clocks with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    /// The error line's timestamp.
    pub from_ms: u64,
    /// Its next real turn (or its revival in the live list); `None` while
    /// it is still stopped.
    #[serde(default)]
    pub to_ms: Option<u64>,
    /// `no network`, `usage limit`, `login expired`, `timed out`,
    /// `API error`.
    pub reason: String,
}

/// How many stop spans a row keeps; older ones are long before any clock.
const MAX_STOPS: usize = 32;

/// The words for an API error line: its `error` code first, else its text.
fn stop_reason(v: &Value) -> String {
    let text = v
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    match v.get("error").and_then(Value::as_str) {
        Some("rate_limit") => "usage limit",
        Some("authentication_failed") => "login expired",
        _ if has(&[
            "EAI_AGAIN",
            "ENOTFOUND",
            "ECONNREFUSED",
            "ECONNRESET",
            "Can't reach",
            "Connection lost",
            "Connection error",
        ]) =>
        {
            "no network"
        }
        _ if has(&["timed out", "Timed out", "timeout"]) => "timed out",
        _ if has(&["limit"]) => "usage limit",
        _ if has(&["/login", "authenticate"]) => "login expired",
        _ => "API error",
    }
    .to_string()
}

/// How far back from the end a first read starts. A tool result can be
/// large, so this is generous; a transcript whose last tool call is further
/// back than this reports no activity, which is honest enough.
const TAIL_BYTES: u64 = 512 * 1024;

/// Read the tail of an agent transcript once. A worker that is still
/// running is followed with a [`TranscriptTail`] instead.
pub fn read_activity(path: &Path) -> Activity {
    let mut tail = TranscriptTail::new(path);
    tail.poll();
    tail.act
}

/// Follows one worker's transcript: the first [`poll`] reads its last
/// [`TAIL_BYTES`], every later one only the bytes appended since, so
/// keeping a running worker's tokens and activity current every second
/// costs a `stat` while it is thinking and a few lines' parse when it
/// writes.
///
/// [`poll`]: TranscriptTail::poll
#[derive(Debug, Clone, Default)]
pub struct TranscriptTail {
    path: PathBuf,
    /// Bytes consumed; `None` before the first read.
    offset: Option<u64>,
    /// A line not yet ended by `\n`.
    partial: Vec<u8>,
    act: Activity,
}

impl TranscriptTail {
    pub fn new(path: impl Into<PathBuf>) -> TranscriptTail {
        TranscriptTail {
            path: path.into(),
            ..TranscriptTail::default()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn activity(&self) -> &Activity {
        &self.act
    }

    /// Read what was appended. Returns true when the activity changed. A
    /// file that shrank (rewritten) is read again as new.
    pub fn poll(&mut self) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return false;
        };
        let (from, fresh) = match self.offset {
            Some(o) if len >= o => (o, false),
            _ => (len.saturating_sub(TAIL_BYTES), true),
        };
        if fresh {
            self.partial.clear();
            self.act = Activity::default();
        }
        if len == from && !fresh {
            return false;
        }
        if file.seek(SeekFrom::Start(from)).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        if file.take(len - from).read_to_end(&mut bytes).is_err() {
            return false;
        }
        self.offset = Some(from + bytes.len() as u64);
        let mut chunk = std::mem::take(&mut self.partial);
        chunk.extend_from_slice(&bytes);
        let mut lines = &chunk[..];
        if fresh && from > 0 {
            // The first line is cut; drop it rather than half-parse it.
            match lines.iter().position(|&b| b == b'\n') {
                Some(i) => lines = &lines[i + 1..],
                None => lines = &[],
            }
        }
        let cut = lines.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let before = self.act.clone();
        for line in lines[..cut].split(|&b| b == b'\n') {
            fold_line(&mut self.act, line);
        }
        self.partial = lines[cut..].to_vec();
        self.act != before
    }
}

/// Fold one transcript line into `act`, oldest line first.
fn fold_line(act: &mut Activity, line: &[u8]) {
    // Only an assistant line says anything, and most lines are not one:
    // skip the parse for those.
    const ASSISTANT: &[u8] = b"\"assistant\"";
    if line.len() < ASSISTANT.len() || !line.windows(ASSISTANT.len()).any(|w| w == ASSISTANT) {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let ts = as_millis(&v, "timestamp");
    if let Some(ms) = ts {
        act.last_turn_ms = Some(ms);
    }
    let Some(msg) = v.get("message") else {
        return;
    };
    // Claude Code's own messages (an interrupt, an API error) are not a
    // reply anything was billed for, and say nothing about the context.
    if msg.get("model").and_then(Value::as_str) == Some("<synthetic>") {
        // An API error is the worker stopping, until its next real turn.
        let open = act.stops.last().is_some_and(|s| s.to_ms.is_none());
        if v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
            && !open
            && let Some(from_ms) = ts
        {
            act.stops.push(Stop {
                from_ms,
                to_ms: None,
                reason: stop_reason(&v),
            });
            if act.stops.len() > MAX_STOPS {
                act.stops.remove(0);
            }
        }
        return;
    }
    if let Some(open) = act.stops.last_mut().filter(|s| s.to_ms.is_none()) {
        open.to_ms = Some(ts.unwrap_or(open.from_ms).max(open.from_ms));
    }
    if let Some(m) = as_str(msg, "model") {
        act.model = Some(m);
    }
    if let Some(ctx) = msg.get("usage").map(context_tokens)
        && ctx > 0
    {
        act.tokens = Some(ctx);
    }
    if let Some(tu) = msg
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .rev()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("tool_use"))
        })
    {
        act.doing = Some(describe_tool_use(tu));
    }
}

/// The context one `usage` says the worker carries: input, cache creation
/// and cache read, added. Claude Code measures a turn that ran several API
/// iterations by the last real one (not an `advisor_message` or a
/// `compaction`), and falls back to the top level on anything unexpected.
fn context_tokens(u: &Value) -> u64 {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let top = n(u, "input_tokens")
        + n(u, "cache_creation_input_tokens")
        + n(u, "cache_read_input_tokens");
    let Some(its) = u.get("iterations").and_then(Value::as_array) else {
        return top;
    };
    if top == 0 {
        return top;
    }
    let last = its.iter().rfind(|it| {
        it.is_object()
            && !matches!(
                it.get("type").and_then(Value::as_str),
                Some("advisor_message" | "compaction")
            )
    });
    let Some(last) = last else { return top };
    if !matches!(
        last.get("type").and_then(Value::as_str),
        Some("message" | "fallback_message")
    ) {
        return top;
    }
    let mut sum = 0u64;
    for k in [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ] {
        match last.get(k).and_then(Value::as_f64) {
            Some(x) if x >= 0.0 => {
                if k != "output_tokens" {
                    sum += x as u64;
                }
            }
            _ => return top,
        }
    }
    if sum == 0 { top } else { sum }
}

/// Longest `doing` line, in characters.
const DOING_MAX: usize = 80;

/// `Tool: the argument that says what it is doing`, one line.
pub fn describe_tool_use(tool_use: &Value) -> String {
    let name = as_str(tool_use, "name").unwrap_or_else(|| "tool".into());
    let input = tool_use.get("input").unwrap_or(&Value::Null);
    let arg = [
        "description",
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "skill",
        "to",
        "prompt",
    ]
    .iter()
    .find_map(|k| as_str(input, k));
    let line = match arg {
        Some(a) => format!("{name}: {a}"),
        None => name,
    };
    one_line(&line, DOING_MAX)
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max - 1).collect::<String>() + "…"
    } else {
        flat
    }
}

// ---------------------------------------------------------------------------
// Completion notifications in the parent transcript
// ---------------------------------------------------------------------------

/// A `<task-notification>` for one worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    pub outcome: Outcome,
    /// When the notification was queued — the landing time.
    pub at_ms: u64,
    /// `<summary>`: `Agent "…" finished`.
    pub summary: Option<String>,
}

/// Pull `<tag>…</tag>` out of `text`.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

/// Parse one transcript line for a notification. The same notification is
/// written up to three times (queue enqueue, attachment, user turn) with
/// timestamps milliseconds apart; the caller keeps the latest.
fn completion_in_line(line: &str) -> Option<(String, Completion)> {
    if line.contains("\"toolUseResult\"")
        && line.contains("\"agentId\"")
        && let Some(found) = foreground_result(line)
    {
        return Some(found);
    }
    if !line.contains("<task-notification>") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    // A dequeue ("remove") is the queue letting go, not a new landing.
    if v.get("operation").and_then(Value::as_str) == Some("remove") {
        return None;
    }
    let at_ms = as_millis(&v, "timestamp")?;
    // The text sits in one of three places depending on the line's type; a
    // string search over the serialised value finds it in all of them
    // without binding to any.
    let texts = collect_strings(&v);
    let text = texts.iter().find(|s| s.contains("<task-notification>"))?;
    let body = tag(text, "task-notification")?;
    let id = tag(body, "task-id")?.to_string();
    if id.is_empty() {
        return None;
    }
    let outcome = tag(body, "status")
        .and_then(Outcome::parse)
        .unwrap_or(Outcome::Unknown);
    let summary = tag(body, "summary").map(str::to_string);
    Some((
        id,
        Completion {
            outcome,
            at_ms,
            summary,
        },
    ))
}

/// A foreground worker sends no notification: the Agent tool call simply
/// returns, and the parent's `tool_result` line carries `toolUseResult` with
/// the `agentId` and a `status`. A background spawn writes the same shape at
/// launch with `status: "async_launched"`, which is not a landing.
fn foreground_result(line: &str) -> Option<(String, Completion)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let r = v.get("toolUseResult")?;
    let id = as_str(r, "agentId")?;
    let status = as_str(r, "status").unwrap_or_default();
    if status == "async_launched" {
        return None;
    }
    let outcome = Outcome::parse(&status).unwrap_or(Outcome::Unknown);
    Some((
        id,
        Completion {
            outcome,
            at_ms: as_millis(&v, "timestamp")?,
            summary: None,
        },
    ))
}

fn collect_strings(v: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::String(s) => out.push(s.as_str()),
            Value::Array(a) => stack.extend(a),
            Value::Object(o) => stack.extend(o.values()),
            _ => {}
        }
    }
    out
}

/// Follows a parent transcript and collects the latest landing per worker:
/// a `<task-notification>` for a background worker, the Agent tool's result
/// for a foreground one. [`NotificationTail::poll`] reads only what was appended since the
/// last call, so polling a long session every second costs a `stat`.
///
/// Background shells (`run_in_background`) notify through the same channel;
/// their ids are not agent ids, so they never match a row and are harmless.
#[derive(Debug, Clone, Default)]
pub struct NotificationTail {
    path: PathBuf,
    offset: u64,
    partial: String,
    latest: HashMap<String, Completion>,
}

impl NotificationTail {
    pub fn new(path: impl Into<PathBuf>) -> NotificationTail {
        NotificationTail {
            path: path.into(),
            ..NotificationTail::default()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read what was appended. Returns true when anything new was learned.
    /// A file that shrank (rewritten) is read again from the start.
    pub fn poll(&mut self) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return false;
        };
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
            self.latest.clear();
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return false;
        }
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let Some(cut) = self.partial.rfind('\n') else {
            return false;
        };
        let complete: String = self.partial.drain(..=cut).collect();
        let mut changed = false;
        for line in complete.lines() {
            if let Some((id, c)) = completion_in_line(line) {
                let newer = self.latest.get(&id).is_none_or(|old| c.at_ms > old.at_ms);
                if newer {
                    self.latest.insert(id, c);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Latest notification per task id.
    pub fn completions(&self) -> &HashMap<String, Completion> {
        &self.latest
    }
}

/// Every notification in a whole transcript, latest per id. A one-shot
/// [`NotificationTail`].
pub fn read_completions(parent_transcript: &Path) -> HashMap<String, Completion> {
    let mut tail = NotificationTail::new(parent_transcript);
    tail.poll();
    tail.latest
}

/// A worker is done when its latest notification is at least as new as its
/// latest turn. A turn after a notification means it was messaged again and
/// is working. `SLACK_MS` absorbs the few milliseconds by which the final
/// turn and the notification can land out of order.
pub fn is_finished(completion: &Completion, last_turn_ms: Option<u64>) -> bool {
    const SLACK_MS: u64 = 2_000;
    match last_turn_ms {
        Some(last) => completion.at_ms + SLACK_MS >= last,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One subagent, as the pane shows it. The stable type the pane and the
/// relay consume; fields are only ever added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRow {
    /// Agent id: `agent-<id>.jsonl`, `tasks[].id`, `<task-id>`. The join key
    /// with the feed's `agent_id`. Never rewritten.
    pub id: String,
    pub stage: Stage,
    /// Set once Done.
    #[serde(default)]
    pub outcome: Option<Outcome>,
    /// A name, when the agent was given one.
    #[serde(default)]
    pub name: Option<String>,
    /// The Agent tool's `description` — what the pane shows without a feed.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Epoch ms; ELAPSED is drawn from this every second.
    #[serde(default)]
    pub started_ms: Option<u64>,
    /// Epoch ms it landed (notification time, else when it left the list).
    #[serde(default)]
    pub ended_ms: Option<u64>,
    #[serde(default)]
    pub tokens: Option<u64>,
    /// What it is doing now: the live `label` when Claude Code gave one,
    /// else the last tool call in its transcript. Cleared on Done.
    #[serde(default)]
    pub activity: Option<String>,
    /// Its `agent-<id>.jsonl`, once known — what a click opens.
    #[serde(default)]
    pub transcript: Option<PathBuf>,
    /// Its last assistant turn (epoch ms), for revival detection.
    #[serde(default)]
    pub last_turn_ms: Option<u64>,
    /// Summary line of its last notification.
    #[serde(default)]
    pub summary: Option<String>,
    /// `tokens` came from its transcript, not the live list. Once it has,
    /// the live list's `tokenCount` never replaces it: that count is the
    /// context *plus* output so far, and an API error zeroes the context
    /// part, so a failed worker lists at its output alone — the `764`
    /// of the real transcript in the tests.
    #[serde(default)]
    pub tokens_from_transcript: bool,
    /// Claude Code's own count, the live list's `tokenCount`: the context of
    /// its last turn *plus* every output token of the run so far (and an
    /// estimate of a reply still streaming) — the number its agents view
    /// shows. [`SubagentRow::shown_tokens`] is the one the pane draws.
    #[serde(default)]
    pub live_tokens: Option<u64>,
    /// Spans it stood stopped on an API error ([`Stop`]), oldest first.
    #[serde(default)]
    pub stops: Vec<Stop>,
    /// The agent that spawned it, when another agent did (its meta's
    /// `parentAgentId`): the pane draws it under that agent's row.
    /// `None` for the session's own workers.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Its meta's `spawnDepth`: 1 for the session's own workers.
    #[serde(default)]
    pub depth: Option<u32>,
}

impl SubagentRow {
    fn new(id: &str, stage: Stage) -> SubagentRow {
        SubagentRow {
            id: id.to_string(),
            stage,
            outcome: None,
            name: None,
            description: None,
            agent_type: None,
            model: None,
            started_ms: None,
            ended_ms: None,
            tokens: None,
            activity: None,
            transcript: None,
            last_turn_ms: None,
            summary: None,
            tokens_from_transcript: false,
            live_tokens: None,
            stops: Vec::new(),
            parent_id: None,
            depth: None,
        }
    }

    /// Why its clocks stand still, and since when: an API error it has not
    /// written past, else a failed, killed or stopped end. `None` while it
    /// is working — or finished cleanly, where the manager's landing is
    /// still time on the task.
    pub fn stopped(&self) -> Option<(u64, String)> {
        if let Some(s) = self.stops.last().filter(|s| s.to_ms.is_none()) {
            return Some((s.from_ms, format!("stopped: {}", s.reason)));
        }
        if self.stage != Stage::Done {
            return None;
        }
        let word = match self.outcome? {
            Outcome::Failed => "failed",
            Outcome::Killed => "killed",
            Outcome::Stopped => "stopped",
            Outcome::Completed | Outcome::Unknown => return None,
        };
        Some((self.ended_ms?, format!("stopped: {word}")))
    }

    /// How much of `[start, upto)` it stood stopped on API errors, in ms —
    /// taken off its clocks the way a usage-limit hold is.
    pub fn stopped_ms(&self, start: u64, upto: u64) -> u64 {
        self.stops
            .iter()
            .map(|s| {
                let a = s.from_ms.max(start);
                let b = s.to_ms.unwrap_or(upto).min(upto);
                b.saturating_sub(a)
            })
            .sum()
    }

    /// Fold in the spans a transcript read found: a known span (same
    /// start) that is still open takes the read's end — one the live list
    /// already closed at the continue keeps that — and a new one is added.
    fn merge_stops(&mut self, found: &[Stop]) {
        for f in found {
            match self.stops.iter_mut().find(|s| s.from_ms == f.from_ms) {
                Some(s) => {
                    if s.to_ms.is_none() {
                        s.to_ms = f.to_ms;
                    }
                }
                None => self.stops.push(f.clone()),
            }
        }
        self.stops.sort_by_key(|s| s.from_ms);
        // Only the newest may be open: an older one the read no longer
        // reached ended when a later error began, at the latest.
        for i in 1..self.stops.len() {
            if self.stops[i - 1].to_ms.is_none() {
                self.stops[i - 1].to_ms = Some(self.stops[i].from_ms);
            }
        }
        let over = self.stops.len().saturating_sub(MAX_STOPS);
        self.stops.drain(..over);
    }

    /// The live list lists it running again from `start_ms`: an open stop
    /// ends there. True when one did — a resume, not a new run.
    fn resume_at(&mut self, start_ms: Option<u64>) -> bool {
        match (self.stops.last_mut(), start_ms) {
            (Some(s), Some(at)) if s.to_ms.is_none() && at > s.from_ms => {
                s.to_ms = Some(at);
                true
            }
            _ => false,
        }
    }

    /// The join key with the feed's `agent_id` (same as [`SubagentRow::id`]).
    /// With [`SubagentRow::running`], [`SubagentRow::started_ms`] and
    /// [`SubagentRow::tokens`] this is everything `feed::LiveAgent` asks of a
    /// live row, so its impl is one line per method.
    pub fn agent_id(&self) -> &str {
        &self.id
    }

    /// Still running (else it has finished).
    pub fn running(&self) -> bool {
        self.stage == Stage::Running
    }

    /// Name, else description, else the id — never empty.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.description.as_deref())
            .unwrap_or(&self.id)
    }

    /// The count the pane draws: Claude Code's own (`tokenCount`, what its
    /// agents view shows), unless the transcript's context is
    /// larger. By construction Claude Code's is the context plus output, so
    /// it is only ever smaller when it is wrong: after an API error it is
    /// the output alone, and just after a continue it is a
    /// count the next turn replaces. Then the transcript's stands in, as it
    /// does while no live list has named one.
    pub fn shown_tokens(&self) -> Option<u64> {
        match (self.live_tokens, self.tokens) {
            (Some(live), Some(ctx)) => Some(live.max(ctx)),
            (live, ctx) => live.or(ctx),
        }
    }

    /// Wall time so far (Running) or taken (Done), in ms.
    pub fn elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        let start = self.started_ms?;
        let end = match self.stage {
            Stage::Running => now_ms,
            Stage::Done => self.ended_ms.unwrap_or(now_ms),
        };
        Some(end.saturating_sub(start))
    }

    fn mark_done(&mut self, outcome: Outcome, at_ms: u64) {
        self.stage = Stage::Done;
        // A known outcome is never downgraded to Unknown.
        if self.outcome.is_none() || outcome != Outcome::Unknown {
            self.outcome = Some(outcome);
        }
        if self.ended_ms.is_none() || outcome != Outcome::Unknown {
            self.ended_ms = Some(at_ms);
        }
        self.activity = None;
    }

    fn revive(&mut self) {
        self.stage = Stage::Running;
        self.outcome = None;
        self.ended_ms = None;
        self.summary = None;
    }

    fn fill_meta(&mut self, meta: AgentMeta) {
        if self.description.is_none() {
            self.description = meta.description;
        }
        if self.agent_type.is_none() {
            self.agent_type = meta.agent_type;
        }
        if self.model.is_none() {
            self.model = meta.model;
        }
        if meta.parent_id.is_some() {
            self.parent_id = meta.parent_id;
        }
        if meta.depth.is_some() {
            self.depth = meta.depth;
        }
    }
}

/// A native row is what the feed merge calls a live row.
impl crate::feed::LiveAgent for SubagentRow {
    fn agent_id(&self) -> &str {
        SubagentRow::agent_id(self)
    }
    fn running(&self) -> bool {
        SubagentRow::running(self)
    }
    fn started_ms(&self) -> Option<u64> {
        self.started_ms
    }
    fn tokens(&self) -> Option<u64> {
        self.shown_tokens()
    }
    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    fn ended_ms(&self) -> Option<u64> {
        self.ended_ms
    }
    fn last_sign_ms(&self) -> Option<u64> {
        self.last_turn_ms.max(self.started_ms)
    }
    fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }
}

/// The subagents of one tab's Claude session, Running and Done.
///
/// Feed it [`LiveSnapshot`]s as the relay delivers them
/// ([`Tracker::apply_live`]) and call [`Tracker::refresh`] about once a
/// second; read [`Tracker::rows`]. Nothing ages out — Done rows stay until
/// [`Tracker::clear`] (the tab's `/clear`, or `SessionStart` with
/// `source=clear`).
///
/// Serialise it with the tab; the on-disk readers (`subagents_dir`, the
/// notification offset) are rebuilt after a restart, from the stored session
/// and config dir, on the first `refresh`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tracker {
    /// Parent session id: the one the rows belong to, the latest seen.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Earlier ids of the same conversation (a resume re-ids it); rows
    /// survive the change.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Claude config dir the session runs under.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    rows: Vec<SubagentRow>,
    #[serde(skip)]
    dirs: Vec<PathBuf>,
    #[serde(skip)]
    tails: Vec<NotificationTail>,
    /// Sessions whose `subagents/` dir has been found.
    #[serde(skip)]
    resolved: Vec<String>,
    /// A follower per Running worker's transcript, by agent id.
    #[serde(skip)]
    followers: HashMap<String, TranscriptTail>,
    /// Finished workers whose transcript this process has read once: a row
    /// restored with the live list's count (or none) is set right from its
    /// transcript after a restart, and then left alone.
    #[serde(skip)]
    checked: std::collections::HashSet<String>,
    /// Workers whose `agent-<id>.meta.json` this process has read: once
    /// each, as a worker the live list announced first (or one restored from
    /// before its parent was kept) has its parent only there.
    #[serde(skip)]
    meta_read: std::collections::HashSet<String>,
    /// Workers whose Done rows were cleared by hand ([`Tracker::clear_done`]):
    /// not brought back by the disk or the live list unless they run again.
    #[serde(default)]
    dismissed: Vec<String>,
    /// When the Done rows were last cleared by hand: the pane hides the
    /// feed's Done rows that landed before it too.
    #[serde(default)]
    pub done_cleared_ms: Option<u64>,
}

impl Tracker {
    pub fn new(config_dir: Option<PathBuf>) -> Tracker {
        Tracker {
            config_dir,
            ..Tracker::default()
        }
    }

    /// All rows: Running first (oldest start first), then Done (most recent
    /// landing first).
    pub fn rows(&self) -> &[SubagentRow] {
        &self.rows
    }

    /// The row for an agent id — the feed merge's lookup.
    pub fn get(&self, id: &str) -> Option<&SubagentRow> {
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Forget every row (the tab's `/clear`). Keeps the session binding.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Clear the Done rows by hand (`giverny manage clear-done`),
    /// keeping the Running ones. A cleared worker stays gone — the next
    /// refresh finds its transcript and notification still on disk — unless
    /// it runs again. Returns how many rows went.
    pub fn clear_done(&mut self, now_ms: u64) -> usize {
        let before = self.rows.len();
        for r in &self.rows {
            if r.stage == Stage::Done && !self.dismissed.contains(&r.id) {
                self.dismissed.push(r.id.clone());
            }
        }
        self.rows.retain(|r| r.stage != Stage::Done);
        self.done_cleared_ms = Some(now_ms);
        before - self.rows.len()
    }

    /// The session these rows belong to was stopped from its tab:
    /// its workers died with it, so every Running row is
    /// Done as of `now_ms`, `Stopped`. Returns how many were.
    pub fn end_running(&mut self, now_ms: u64) -> usize {
        let mut ended = 0;
        for r in &mut self.rows {
            if r.stage == Stage::Running {
                r.mark_done(Outcome::Stopped, now_ms);
                ended += 1;
            }
        }
        if ended > 0 {
            self.sort();
        }
        ended
    }

    /// Has anything worth keeping over a restart: rows, or a clear that has
    /// to keep holding.
    pub fn worth_saving(&self) -> bool {
        !self.rows.is_empty() || !self.dismissed.is_empty()
    }

    /// Is `session` the conversation these rows belong to? `Some(true)` for
    /// the bound id or an alias, or a transcript with the same root
    /// ([`conversation_root`]) as one of them; `Some(false)` for one whose
    /// root is another; `None` when either side has no root to compare
    /// (no transcript yet, or only headers).
    pub fn continues(&self, session: &str) -> Option<bool> {
        if self.session_id.as_deref() == Some(session) || self.aliases.iter().any(|a| a == session)
        {
            return Some(true);
        }
        let config = self.config_dir.as_deref()?;
        let theirs = conversation_root(&session_transcript(config, session)?)?;
        let mut any = false;
        for sid in self.session_id.iter().chain(self.aliases.iter()) {
            let Some(root) = session_transcript(config, sid).and_then(|p| conversation_root(&p))
            else {
                continue;
            };
            if root == theirs {
                return Some(true);
            }
            any = true;
        }
        any.then_some(false)
    }

    /// Bind to a session. A different id keeps the rows and records the old
    /// one as an alias — a resumed or re-id'd conversation is still the same
    /// session; call [`Tracker::clear`] as well if it is not.
    pub fn set_session(&mut self, session_id: &str) {
        if let Some(cur) = &self.session_id {
            if cur == session_id {
                return;
            }
            if !self.aliases.contains(cur) {
                self.aliases.push(cur.clone());
            }
        }
        self.aliases.retain(|a| a != session_id);
        self.session_id = Some(session_id.to_string());
    }

    fn row_mut(&mut self, id: &str, stage: Stage) -> &mut SubagentRow {
        if let Some(i) = self.rows.iter().position(|r| r.id == id) {
            &mut self.rows[i]
        } else {
            self.rows.push(SubagentRow::new(id, stage));
            self.rows.last_mut().expect("just pushed")
        }
    }

    /// Fold in one live list. Every listed task becomes (or stays) a row;
    /// a Running row absent from the list is Done as of `now_ms`, with its
    /// outcome to be filled by a notification.
    pub fn apply_live(&mut self, snap: &LiveSnapshot, now_ms: u64) {
        if let Some(sid) = &snap.session_id {
            self.set_session(sid);
        }
        for t in &snap.tasks {
            if let Some(i) = self.dismissed.iter().position(|d| *d == t.id) {
                // Cleared by hand: still listed finished for a while, which
                // must not bring it back; running again, it is a row again.
                if Outcome::parse(&t.status).is_some() {
                    continue;
                }
                self.dismissed.remove(i);
            }
            let row = self.row_mut(&t.id, Stage::Running);
            if t.name.is_some() {
                row.name = t.name.clone();
            }
            if t.description.is_some() {
                row.description = t.description.clone();
            }
            if t.model.is_some() {
                row.model = t.model.clone();
            }
            // A worker continued after an API error is listed afresh, its
            // start moved to the continue: the stop ends there and the
            // clock carries on from where it stood.
            let resumed = Outcome::parse(&t.status).is_none() && row.resume_at(t.start_ms);
            if t.start_ms.is_some() && !(resumed && row.started_ms.is_some()) {
                row.started_ms = t.start_ms;
            }
            // The transcript's count is the one the pane keeps once there
            // is one (it moves every second, and agrees with the
            // manager's); the live list's stands in until then. It is
            // never taken back: after an API error the live count is the
            // output alone.
            if t.tokens.is_some() && !row.tokens_from_transcript {
                row.tokens = t.tokens;
            }
            // Claude Code writes 0 for a worker with no progress yet.
            if let Some(n) = t.tokens.filter(|&n| n > 0) {
                row.live_tokens = Some(n);
            }
            match Outcome::parse(&t.status) {
                None => {
                    if row.stage == Stage::Done {
                        row.revive();
                    }
                    // The transcript's last tool call is kept once there
                    // is one: it is re-read every second, where the label
                    // comes every few and often just repeats the
                    // description. The label stands in until then.
                    if t.label.is_some() && (row.transcript.is_none() || row.activity.is_none()) {
                        row.activity = t.label.clone();
                    }
                }
                // Claude Code lists a finished task for a while after: it
                // ended when first listed so (or when its notification
                // says), not on every list since (a moving end
                // re-told its manager the task was now really paused).
                Some(o) if row.stage == Stage::Done && row.outcome == Some(o) => {}
                Some(o) => row.mark_done(o, now_ms),
            }
        }
        for row in &mut self.rows {
            if row.stage == Stage::Running && !snap.tasks.iter().any(|t| t.id == row.id) {
                row.mark_done(Outcome::Unknown, now_ms);
            }
        }
        self.sort();
    }

    /// Read the disk: find the session's `subagents/` dir and transcript,
    /// pick up new notifications, refresh each row's activity, meta and
    /// transcript path, and add Done rows for workers that finished while
    /// nothing was watching (Giverny closed, or the pane just switched on).
    ///
    /// Cheap enough for once a second: one `read_dir`, and an appended-bytes
    /// read of the parent transcript and of each Running worker's
    /// ([`TranscriptTail`]) — a `stat` each while nothing was written.
    pub fn refresh(&mut self) {
        let Some(config) = self.config_dir.clone() else {
            return;
        };
        // Resolve each session's dir once; one not found yet (no worker
        // spawned so far) is looked for again next time.
        let sessions: Vec<String> = self
            .session_id
            .iter()
            .chain(self.aliases.iter())
            .filter(|s| !self.resolved.contains(s))
            .cloned()
            .collect();
        for sid in sessions {
            if let Some(dir) = subagents_dir(&config, &sid) {
                if let Some(parent) = dir.parent().and_then(Path::parent) {
                    self.tails
                        .push(NotificationTail::new(parent.join(format!("{sid}.jsonl"))));
                }
                self.dirs.push(dir);
                self.resolved.push(sid);
            }
        }
        for tail in &mut self.tails {
            tail.poll();
        }
        let mut completions: HashMap<String, Completion> = HashMap::new();
        for tail in &self.tails {
            for (id, c) in tail.completions() {
                if completions.get(id).is_none_or(|old| c.at_ms > old.at_ms) {
                    completions.insert(id.clone(), c.clone());
                }
            }
        }
        let dirs = self.dirs.clone();
        for dir in &dirs {
            for id in list_agent_ids(dir) {
                let known = self.rows.iter().any(|r| r.id == id);
                if !known && self.dismissed.contains(&id) {
                    // Cleared by hand; the live list says if it runs again.
                    continue;
                }
                let completion = completions.get(&id);
                if !known && completion.is_none() {
                    // Not in the live list and never notified: a worker the
                    // live list will announce, or one killed with its parent.
                    // Either way not ours to invent.
                    continue;
                }
                let unchecked = !self.checked.contains(&id);
                let want_meta = !self.meta_read.contains(&id);
                let needs_read = !known
                    || self.rows.iter().any(|r| {
                        r.id == id
                            && (r.stage == Stage::Running
                                || r.transcript.is_none()
                                || (unchecked && !r.tokens_from_transcript))
                    });
                if !needs_read && completion.is_none() && !want_meta {
                    continue;
                }
                let path = agent_transcript(dir, &id);
                let row_is_running = self
                    .rows
                    .iter()
                    .any(|r| r.id == id && r.stage == Stage::Running);
                // A Running worker is followed: only what it appended since
                // the last second is read. Anything else is read once.
                let act = if row_is_running {
                    let f = self
                        .followers
                        .entry(id.clone())
                        .or_insert_with(|| TranscriptTail::new(&path));
                    f.poll();
                    Some(f.activity().clone())
                } else if needs_read {
                    self.checked.insert(id.clone());
                    Some(read_activity(&path))
                } else {
                    None
                };
                let meta = want_meta.then(|| read_meta(dir, &id));
                // Written at the spawn, but a worker's transcript can show
                // up first: one with no meta yet is looked at again.
                if meta.as_ref().is_some_and(|m| *m != AgentMeta::default()) {
                    self.meta_read.insert(id.clone());
                }
                // A worker new to us is Done unless it has written since its
                // notification: messaged again and still going, with no live
                // entry yet — Running until the list or the next
                // notification says otherwise.
                let initial = match (completion, &act) {
                    (Some(c), Some(a)) if !is_finished(c, a.last_turn_ms) => Stage::Running,
                    _ => Stage::Done,
                };
                let row = self.row_mut(&id, initial);
                row.transcript = Some(path);
                if let Some(meta) = meta {
                    row.fill_meta(meta);
                }
                if let Some(act) = act {
                    if act.last_turn_ms.is_some() {
                        row.last_turn_ms = act.last_turn_ms;
                    }
                    if row.model.is_none() {
                        row.model = act.model;
                    }
                    // The transcript's count wins at every stage: a Done row
                    // holding the live list's (the output alone, after an API
                    // error) is set right the first time it is read.
                    if act.tokens.is_some() {
                        row.tokens = act.tokens;
                        row.tokens_from_transcript = true;
                    }
                    row.merge_stops(&act.stops);
                    // The transcript's last tool call; the live label
                    // stands until the worker has made one.
                    if row.stage == Stage::Running && act.doing.is_some() {
                        row.activity = act.doing;
                    }
                }
                if !known && row.started_ms.is_none() {
                    row.started_ms = first_line_ms(&agent_transcript(dir, &id));
                }
                // A worker woken by a message is listed afresh with its
                // start moved to the message, minutes before its first
                // turn after it: that listing is newer than the notice
                // too. Its last turn alone would land it
                // here and revive it at the next live list, every few
                // seconds, until it next wrote.
                let alive = match (row.last_turn_ms, row.started_ms) {
                    (Some(t), Some(s)) => Some(t.max(s)),
                    (t, s) => t.or(s),
                };
                if let Some(c) = completion
                    && is_finished(c, alive)
                {
                    row.mark_done(c.outcome, c.at_ms);
                    row.summary = c.summary.clone();
                }
            }
        }
        let rows = &self.rows;
        self.followers.retain(|id, _| {
            rows.iter()
                .any(|r| &r.id == id && r.stage == Stage::Running)
        });
        self.sort();
    }

    fn sort(&mut self) {
        self.rows.sort_by(|a, b| match (a.stage, b.stage) {
            (Stage::Running, Stage::Done) => std::cmp::Ordering::Less,
            (Stage::Done, Stage::Running) => std::cmp::Ordering::Greater,
            (Stage::Running, Stage::Running) => a
                .started_ms
                .unwrap_or(u64::MAX)
                .cmp(&b.started_ms.unwrap_or(u64::MAX))
                .then_with(|| a.id.cmp(&b.id)),
            (Stage::Done, Stage::Done) => b.ended_ms.cmp(&a.ended_ms).then_with(|| a.id.cmp(&b.id)),
        });
    }
}

/// Timestamp of the first line of a transcript — its start, when no live
/// list ever said.
fn first_line_ms(path: &Path) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().take(20) {
        let Ok(line) = line else { break };
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some(ms) = as_millis(&v, "timestamp")
        {
            return Some(ms);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("giverny-subagents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const T0: u64 = 1_790_000_000_000;

    fn iso(ms: u64) -> String {
        jiff::Timestamp::from_millisecond(ms as i64)
            .unwrap()
            .to_string()
    }

    fn notification_lines(id: &str, status: &str, ms: u64) -> String {
        let text = format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<status>{status}</status>\n<summary>Agent \"x\" finished</summary>\n</task-notification>"
        );
        [
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "timestamp": iso(ms), "content": text}),
            serde_json::json!({"type": "queue-operation", "operation": "remove", "timestamp": iso(ms + 900), "content": text}),
            serde_json::json!({"type": "user", "timestamp": iso(ms + 30), "message": {"role": "user", "content": text}}),
        ]
        .iter()
        .map(|v| v.to_string() + "\n")
        .collect()
    }

    /// config/projects/-w/<sid>.jsonl + <sid>/subagents/
    fn layout(name: &str, sid: &str) -> (PathBuf, PathBuf, PathBuf) {
        let config = scratch(name);
        let proj = config.join("projects").join("-w");
        let subs = proj.join(sid).join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        let parent = proj.join(format!("{sid}.jsonl"));
        std::fs::write(&parent, "{\"type\":\"user\"}\n").unwrap();
        (config, subs, parent)
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn parses_live_snapshot_tolerantly() {
        let snap = LiveSnapshot::parse(
            r#"{"session_id":"s1","columns":179,"tasks":[
                {"id":"a1","type":"local_agent","status":"running","description":"Work acme#74",
                 "label":"Grepping","startTime":1790089835667,"model":"m","tokenCount":124832,"cwd":"/w"},
                {"id":"a2","status":"completed","startTime":"2026-09-23T08:00:00Z","tokenCount":"oops"},
                {"status":"running"}
            ]}"#,
        );
        assert_eq!(snap.session_id.as_deref(), Some("s1"));
        assert_eq!(snap.tasks.len(), 2, "entry with no id skipped");
        assert_eq!(snap.tasks[0].label.as_deref(), Some("Grepping"));
        assert_eq!(snap.tasks[0].tokens, Some(124832));
        assert!(snap.tasks[0].is_running());
        assert!(!snap.tasks[1].is_running());
        assert!(snap.tasks[1].start_ms.is_some(), "RFC 3339 start accepted");
        assert_eq!(
            snap.tasks[1].tokens, None,
            "wrong type costs the field only"
        );
        assert_eq!(LiveSnapshot::parse("not json"), LiveSnapshot::default());
    }

    #[test]
    fn notifications_keep_the_latest_per_id() {
        let (_c, _s, parent) = layout("notif", "s1");
        append(&parent, &notification_lines("a1", "completed", T0));
        append(&parent, &notification_lines("a2", "failed", T0 + 10));
        append(&parent, &notification_lines("a1", "killed", T0 + 5000));
        let got = read_completions(&parent);
        assert_eq!(got.len(), 2);
        assert_eq!(got["a1"].outcome, Outcome::Killed);
        assert_eq!(
            got["a1"].at_ms,
            T0 + 5030,
            "latest of the copies, dequeue ignored"
        );
        assert_eq!(got["a2"].outcome, Outcome::Failed);
        assert_eq!(got["a2"].summary.as_deref(), Some("Agent \"x\" finished"));
    }

    #[test]
    fn finished_means_notified_after_the_last_turn() {
        let c = Completion {
            outcome: Outcome::Completed,
            at_ms: T0,
            summary: None,
        };
        assert!(is_finished(&c, Some(T0 - 5000)));
        assert!(is_finished(&c, Some(T0 + 500)), "within slack");
        assert!(!is_finished(&c, Some(T0 + 60_000)), "messaged again");
        assert!(is_finished(&c, None));
    }
}
