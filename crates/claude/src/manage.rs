//! `giverny manage`: the management-panel feed's own writer.
//!
//! The pane draws what a feed file says (`docs/management-panel.md`); this is the
//! writer that ships with Giverny, so a managing Claude session needs
//! nothing else to show Running, Next up with ETAs, and Done. A task is any
//! short name. Every stamp is the machine's clock at the moment the command
//! runs, so ELAPSED and the Done row's `(+5m)` are measured, not reported.
//!
//! The feed file *is* the state: each command reads it, changes one row and
//! writes it back atomically, under a lock, since workers re-estimate their
//! own rows while the dispatcher stamps others. A feed some other writer owns
//! (its `writer` field names someone else, e.g. another manager's) is
//! never touched.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};

use crate::worker_log::{self, WorkerLog};
use crate::{continuation, feed, manage_history, names, resources, sleeps};

/// Is a session a manager: the manage skill ran in it. Rows
/// it writes never make it one ([`crate::managers`]).
pub use crate::managers::is_manager;

/// Marks the files this writer owns, so it never rewrites another's.
pub const WRITER: &str = "giverny/manage";

/// Claude Code exports the session id into every Bash command it runs, and a
/// subagent inherits its dispatcher's: a worker re-estimating its own row
/// writes into its dispatcher's file.
pub const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

pub const USAGE: &str = "\
usage: giverny manage <command> [args] [--session <id>]

  plan  <task> --eta <dur> [--gut <dur>] [--title T] [--note N] [--brief FILE]
               [--repo R] [--after <task>]
                              queue a task (a Next up row) with its estimate;
                              --after: it waits for that task to land
                              (`--after \"\"` clears it)
  hold  <task> --why \"<reason>\"
                              a planned task waits on the dispatcher's own
                              call: the reason shows on its row; tell the user
  unhold <task>               end the hold (also `release-hold`)
  start <task> [--eta <dur> [--gut <dur>]] [--title T] [--agent <id> [--heavy-ok]]
               [--note N] [--brief FILE] [--repo R]
                              the task's worker is starting now (Running);
                              --agent hands it to a worker that already holds
                              another task, refused for one carrying over 100k
                              context unless --heavy-ok; on a landed task,
                              --agent <its own worker> reopens it once that
                              worker is woken (the time since it landed is
                              left out)
  eta   <task> <dur left> [--gut <dur>] [--note N]
                              [--why wait|blocked|scope|load|ready]
                              [--title T] [--agent <id>] [--repo R]
                              re-estimate: this much is left from now (a task
                              with no row is started now, Running, with it);
                              --why wait (or blocked) marks the worker waiting
                              until its next eta, and that span is not work;
                              a worker's first eta on a Running row is its
                              re-estimate: taken as given and scored;
                              --gut <dur> is the raw figure behind any
                              figure given (default: the figure); --agent
                              moves the row to that worker, as on `start`
  land  <task> [--outcome Done|Blocked|...] [--review TEXT] [--note N]
                              the task landed now (Done). Refused, for every
                              outcome and with no override, while anything of
                              it still runs: its worker mid-turn or asleep on
                              its own shells, a `run` command of it, a
                              background shell of its worker; each is listed
                              with how to stop it, and nothing changes
  pause <task> [--note N]     stop the task's clock; `resume <task>` restarts it.
                              It stops nothing else: the task's `run`
                              commands and its worker's background shells
                              still going are listed, each with the
                              command that would stop it
  drop  <task>                remove the task's row; refused as `land` is
                              while anything of it still runs
  name  [<First> | <First Last>]
                              this session's name, printed in full. With no
                              name, the one recorded, else Giverny draws a
                              first name, gives it the surname of this tab's
                              category (else its repo) and records them. A
                              first name alone takes that surname (the same
                              first name again keeps the one recorded); a
                              full name is kept as given
  show                        print the name and the rows
  accuracy [--repo R]         how every estimator's instinct and figures fared
                              against the time really worked, older tasks
                              against recent
  watch                       wait for this session's next progress check (a
                              worker's figure runs out, or it asked for much
                              more time), print it and end; with nothing
                              Running, end at once. Run it in the background
                              while tasks run: its end wakes you. One per
                              session; it only prints
  path                        print the feed file's path
  clear-done                  clear the Done rows: from this session's feed,
                              and from the management panel of the Giverny tab it runs in
  clear                       delete this session's feed

  claim <task> [--ram 3G] [--gpu N --vram 8G] [--slot NAME]...
               [--min-ram 2G] [--priority asap|high|medium|low] [--wait]
                              ask the machine ledger for the memory (and GPUs,
                              slots) the task's worker needs — never cores: CPU
                              is shared and measured. Granted (exit 0), granted
                              smaller (3: less RAM, down to --min-ram), queued
                              behind its holders or while the CPU is busy (4;
                              re-run to poll), refused as larger than the
                              limits (5). --wait: while queued, print the
                              first answer, re-claim every 30s, and end with
                              the final answer and its code; run it in the
                              background, and its end wakes you
  release <task>              give the task's lease back (land and drop do it)
  resources                   capacity, limits, how busy the CPU is, leases
  ask   <task-or-session> \"<msg>\" [--task <yours>] [--priority P]
                              when the ledger is not enough, ask the session
                              holding <task>'s lease (or a session id); the
                              message reaches it on its next tool call
  reply <msg-id> \"<text>\"     answer an ask (or a reply); it reaches the asker
                              the same way
  run   <task> [--ram 3G ...] -- <cmd…>
                              run a worker's heavy command under the task's
                              lease: capped by a systemd scope where there is
                              one (else plain, advisory), its slots locked for
                              the command's life, peak memory and CPU time
                              recorded on the row; exit code = the command's

<task> is any short name (`auth-fix`, `#12`); name it, as a whole word, in the
worker's spawn description too. --brief FILE is what the task's row opens to in
the pane: write the worker's prompt (or at least the task's text) to a file and
pass it on every `plan`; `start` fills it in or replaces it. <dur> is minutes (`25`) or `25m`, `1h30m`, `1.5h`.
The session is --session, else $CLAUDE_CODE_SESSION_ID (set inside Claude Code).
The feed goes to $GIVERNY_FEED_DIR, else <config>/giverny/feeds.

Estimates are told, never corrected: the pane counts down from the figure
given. Every landed task appends its estimate, wall time
and working time (wall minus pauses and waits) to history.jsonl beside the feeds
($GIVERNY_MANAGE_HISTORY overrides; empty turns it off), with the gut figure
behind it (`--gut`: the raw figure, before allowing for how far off your gut
usually runs; default the figure itself). `plan`/`start --eta N` print how
far off your gut on such guesses has run: the median of working time ÷ the
gut figure, over the last 20 tasks of the same kind (repo + the title's
type word, as `BUG:`), else the repo, else all. Allow for it as you judge,
and give both figures. A worker's first `eta` on a running task (its
re-estimate, made after reading the code) is scored the same way, against
the working time that was still to come. `accuracy` shows each track. The
plugin's hook (`giverny hook`) asks a worker to re-estimate five minutes
into its task, with how far off its gut on such re-estimates has run.
A subagent that is no task here has an agent ETA instead (`giverny eta`).

Resources: one ledger for every session on the machine, at
<feed dir>/resources/ledger.json ($GIVERNY_LEDGER overrides). Leases expire
20 minutes after their session's last `giverny manage` command. Limits are
[manager.limits] in Giverny's config.toml, else auto (cores-2, 70% RAM,
90% of each GPU's VRAM). A slot (`cargo:/path/target`) is held by one lease.
Reserve RAM; CPU is shared and measured: a lease holds no cores, every `run`
gets the same CPU weight (the Claude tabs more), and a claim waits on CPU only
while it has been saturated for about a minute with tasks already building.
`run` sets CARGO_BUILD_JOBS from the lease's RAM (a job a GiB) unless set.
--cpu and --min-cpu are accepted and ignored.
`run` with no lease claims one first (the default lease, [management_panel.lease]
in config.toml: 3G unless set; --ram overrides), waits while it is
queued, and releases it when the command ends.
`claim` on a held lease with a smaller --ram/--vram shrinks it in place.
Messages go to <feed dir>/inbox/<session>.jsonl; the plugin's hook delivers
them, and renews the calling session's leases, on every tool call.";

/// One `giverny manage` command, parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Plan(String),
    Start(String),
    Eta(String, u64),
    Land(String),
    Pause(String),
    Resume(String),
    Drop(String),
    /// A planned task waits on the dispatcher, for the reason in `--why`.
    Hold(String),
    /// End a hold.
    Unhold(String),
    /// The session's own name: record it, or (`None`) print it.
    Name(Option<String>),
    Show,
    Path,
    ClearDone,
    Clear,
    /// Ask the machine ledger for a lease.
    Claim(String),
    Release(String),
    Resources,
    /// Run a command under the task's lease.
    Run(String),
    /// Ask the session holding a lease: target, message.
    Ask(String, String),
    /// Answer a message: its id, the text.
    Reply(String, String),
    /// How each estimator's figures fared, older against recent.
    Accuracy,
    /// Wait for the session's next progress check, print it and end:
    /// the manager runs it in the background.
    Watch,
}

impl Cmd {
    /// Does it change a feed, the ledger or an inbox (all but the looks)?
    pub fn changes(&self) -> bool {
        !matches!(self, Cmd::Show | Cmd::Path | Cmd::Accuracy | Cmd::Resources)
    }
}

/// The flags any command may carry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flags {
    pub eta_s: Option<u64>,
    /// `--gut`: the giver's raw figure behind `--eta` (or `eta`'s minutes),
    /// before allowing for how far off its gut usually runs.
    /// The history learns from it; the pane shows the other.
    pub gut_s: Option<u64>,
    pub title: Option<String>,
    pub note: Option<String>,
    pub brief: Option<String>,
    pub agent: Option<String>,
    /// `start --agent`: hand the task to a worker over
    /// [`REUSE_MAX_TOKENS`] all the same.
    pub heavy_ok: bool,
    pub outcome: Option<String>,
    pub review: Option<String>,
    pub session: Option<String>,
    /// `eta --why`: `wait` / `blocked` open a waiting span.
    pub why: Option<String>,
    /// The repo the task is from, when the key and directory do not say.
    pub repo: Option<String>,
    /// `plan --after`: the task this one waits for (empty clears it).
    pub after: Option<String>,
    /// `claim`: what the worker needs.
    pub ram_mb: Option<u64>,
    pub gpu: Option<u32>,
    pub vram_mb: Option<u64>,
    pub slots: Vec<String>,
    pub min_ram_mb: Option<u64>,
    pub priority: Option<String>,
    /// `--cpu`/`--min-cpu` were given: accepted and ignored, with a note
    /// — stale skills and briefs still pass them.
    pub cpu_ignored: bool,
    /// `run`: the command, everything after `--`.
    pub command: Vec<String>,
    /// `ask`: the asker's own task, when the ledger does not make it plain.
    pub task: Option<String>,
    /// `claim --wait`: re-claim while queued, until granted or refused.
    pub wait: bool,
    /// Set by [`run_in`], not parsed: the `--agent` worker's spawn
    /// description, which names the rows a dispatcher started for it before
    /// its id was known.
    pub agent_desc: Option<String>,
    /// The Claude config dir to read spawn descriptions from: tests set it;
    /// else `$CLAUDE_CONFIG_DIR`, `$GIVERNY_PROFILE_DIR`, `~/.claude`.
    pub claude_dir: Option<PathBuf>,
    /// Set by [`tell_record`], not parsed: the factor the giver of this
    /// command's guess had been shown for its track, kept on
    /// the row as `eta_told_x`. `None` is ×1.
    pub told_x: Option<f64>,
    /// Set by [`tell_record`], not parsed: the level of the instinct line
    /// this command prints for its guess, kept on the row as `guess_level`
    /// so a later figure of the session knows its giver has seen that line.
    pub guess_level: Option<String>,
    /// Set by [`tell_record`], not parsed: the factor a worker's `eta` reply
    /// tells it, stamped on the row as `track_x` after the
    /// figure is kept, so the figure it settles on next is known to have
    /// been given under it.
    pub reest_x: Option<f64>,
}

/// How long after an `eta` reply asked for the figure a worker settles on
/// a follow-up with `--gut` takes the first figure's place as
/// the scored re-estimate, rather than counting as a later one.
pub const SETTLE_MS: u64 = 10 * 60 * 1000;

impl Flags {
    /// The ledger request these flags make: no RAM unless said.
    pub fn request(&self) -> resources::Request {
        resources::Request {
            ram_mb: self.ram_mb.unwrap_or(0),
            gpu: self.gpu.unwrap_or(0),
            vram_mb: self.vram_mb.unwrap_or(0),
            slots: self.slots.clone(),
            min_ram_mb: self.min_ram_mb,
            priority: self.priority.clone(),
            peak_mb: None,
        }
    }

    /// The one line said when `--cpu`/`--min-cpu` were given, else `None`.
    pub fn cpu_note(&self) -> Option<&'static str> {
        self.cpu_ignored.then_some(CPU_IGNORED)
    }
}

/// What `claim` and `run` say to a `--cpu` or `--min-cpu`.
pub const CPU_IGNORED: &str = "--cpu/--min-cpu ignored: a lease reserves RAM only; CPU is shared \
     and measured (a claim waits while it is saturated)";

fn parse_mem(name: &str, v: &str) -> Result<u64, String> {
    giverny_core::limits::Mem::parse(v)
        .map(|m| m.0)
        .ok_or_else(|| format!("{name}: bad size {v} (e.g. 3G, 512M)"))
}

fn parse_count(name: &str, v: &str) -> Result<u32, String> {
    v.trim()
        .parse()
        .map_err(|_| format!("{name}: not a whole number: {v}"))
}

/// `25` (minutes), `25m`, `90s`, `1h30m`, `1.5h` → seconds.
pub fn parse_dur(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('~').to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Ok(m) = s.parse::<f64>() {
        return (m.is_finite() && m >= 0.0).then(|| (m * 60.0).round() as u64);
    }
    let (mut total, mut num) = (0.0f64, String::new());
    for c in s.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'h' | 'm' | 's' | 'd' => {
                let n: f64 = num.parse().ok()?;
                num.clear();
                total += n * match c {
                    'd' => 86400.0,
                    'h' => 3600.0,
                    'm' => 60.0,
                    _ => 1.0,
                };
            }
            ' ' => {}
            _ => return None,
        }
    }
    (num.is_empty() && total.is_finite()).then(|| total.round() as u64)
}

/// Parse `giverny manage …`'s arguments (after
/// `manage`).
pub fn parse_args(args: &[String]) -> Result<(Cmd, Flags), String> {
    let mut flags = Flags::default();
    let mut pos: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            flags.command = it.by_ref().cloned().collect();
            break;
        }
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--eta" => {
                let v = val("--eta")?;
                flags.eta_s = Some(parse_dur(&v).ok_or_else(|| format!("bad duration: {v}"))?);
            }
            "--gut" => {
                let v = val("--gut")?;
                flags.gut_s = Some(parse_dur(&v).ok_or_else(|| format!("bad duration: {v}"))?);
            }
            "--title" => flags.title = Some(val("--title")?),
            "--note" => flags.note = Some(val("--note")?),
            "--brief" => {
                // The pane reads it from its own directory: keep it absolute.
                let v = val("--brief")?;
                flags.brief = Some(
                    std::path::absolute(&v)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or(v),
                );
            }
            "--agent" => flags.agent = Some(val("--agent")?),
            "--heavy-ok" => flags.heavy_ok = true,
            "--outcome" => flags.outcome = Some(val("--outcome")?),
            "--review" => flags.review = Some(val("--review")?),
            "--session" => flags.session = Some(val("--session")?),
            "--why" => flags.why = Some(val("--why")?),
            "--repo" => flags.repo = Some(val("--repo")?),
            "--after" => flags.after = Some(val("--after")?.trim().to_string()),
            // Old flags, still passed by stale skills and briefs: taken,
            // value and all, and ignored.
            "--cpu" | "--min-cpu" => {
                val(a)?;
                flags.cpu_ignored = true;
            }
            "--gpu" => flags.gpu = Some(parse_count("--gpu", &val("--gpu")?)?),
            "--ram" => flags.ram_mb = Some(parse_mem("--ram", &val("--ram")?)?),
            "--vram" => flags.vram_mb = Some(parse_mem("--vram", &val("--vram")?)?),
            "--min-ram" => flags.min_ram_mb = Some(parse_mem("--min-ram", &val("--min-ram")?)?),
            "--slot" => flags.slots.push(val("--slot")?),
            "--priority" => flags.priority = Some(val("--priority")?),
            "--task" => flags.task = Some(val("--task")?),
            "--wait" => flags.wait = true,
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with("--") => return Err(format!("unknown flag {s}\n\n{USAGE}")),
            _ => pos.push(a.clone()),
        }
    }
    let mut pos = pos.into_iter();
    let verb = pos.next().ok_or_else(|| USAGE.to_string())?;
    let mut task = || {
        pos.next()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("`{verb}` needs a task name\n\n{USAGE}"))
    };
    let cmd = match verb.as_str() {
        "plan" => Cmd::Plan(task()?),
        "start" => Cmd::Start(task()?),
        "eta" => {
            let t = task()?;
            let left = pos
                .next()
                .or_else(|| flags.eta_s.map(|s| format!("{}s", s)))
                .ok_or("`eta` needs the time left, e.g. `giverny manage eta auth-fix 20`")?;
            Cmd::Eta(
                t,
                parse_dur(&left).ok_or_else(|| format!("bad duration: {left}"))?,
            )
        }
        "land" | "done" => Cmd::Land(task()?),
        "pause" => Cmd::Pause(task()?),
        "resume" => Cmd::Resume(task()?),
        "drop" => Cmd::Drop(task()?),
        "hold" => Cmd::Hold(task()?),
        "unhold" | "release-hold" => Cmd::Unhold(task()?),
        "name" => {
            let name = pos.by_ref().collect::<Vec<_>>().join(" ");
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            Cmd::Name((!name.is_empty()).then_some(name))
        }
        "show" => Cmd::Show,
        "path" => Cmd::Path,
        "clear-done" | "clear_done" | "cleardone" => Cmd::ClearDone,
        "clear" => Cmd::Clear,
        "claim" => Cmd::Claim(task()?),
        "release" => Cmd::Release(task()?),
        "resources" => Cmd::Resources,
        "accuracy" => Cmd::Accuracy,
        "watch" => Cmd::Watch,
        "run" => Cmd::Run(task()?),
        "ask" | "reply" => {
            let what = task()?;
            let text = pos.by_ref().collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                return Err(format!(
                    "`{verb}` needs the message, e.g. `giverny manage {verb} {what} \"…\"`"
                ));
            }
            if verb == "ask" {
                Cmd::Ask(what, text)
            } else {
                Cmd::Reply(what, text)
            }
        }
        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    if matches!(cmd, Cmd::Plan(_)) && flags.eta_s.is_none() {
        return Err("`plan` needs --eta: the pane's Next up rows show it".into());
    }
    if matches!(cmd, Cmd::Hold(_)) && flags.why.as_deref().is_none_or(|w| w.trim().is_empty()) {
        return Err(
            "`hold` needs --why \"<reason>\": a planned task waits only on a reason that shows"
                .into(),
        );
    }
    if matches!(cmd, Cmd::Run(_)) && flags.command.is_empty() {
        return Err(format!(
            "`run` needs a command after `--`, e.g. `giverny manage run t -- cargo test`\n\n{USAGE}"
        ));
    }
    if flags.wait && !matches!(cmd, Cmd::Claim(_)) {
        return Err("`--wait` goes only with `claim`".into());
    }
    if flags.gpu.unwrap_or(0) > 0 && flags.vram_mb.is_none() {
        return Err("`--gpu` needs --vram: how much of each GPU the worker needs".into());
    }
    Ok((cmd, flags))
}

pub(crate) fn stamp(ms: u64) -> String {
    jiff::Timestamp::from_second((ms / 1000) as i64)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

pub(crate) fn ms_of(row: &Map<String, Value>, key: &str) -> Option<u64> {
    match row.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s
            .trim()
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond().max(0) as u64),
        _ => None,
    }
}

pub(crate) fn u64_of(row: &Map<String, Value>, key: &str) -> Option<u64> {
    row.get(key).and_then(Value::as_u64)
}

pub(crate) fn stage_of(row: &Map<String, Value>) -> Option<feed::Stage> {
    row.get("stage")
        .and_then(Value::as_str)
        .and_then(feed::Stage::parse)
}

fn stage_word(s: feed::Stage) -> &'static str {
    match s {
        feed::Stage::Running => "running",
        feed::Stage::Planned => "planned",
        feed::Stage::Done => "done",
    }
}

/// A fresh feed document for `session`.
pub fn new_doc(session: &str) -> Value {
    json!({ "version": feed::FEED_VERSION, "session": session, "writer": WRITER, "rows": [] })
}

/// Who wrote this document, when it says.
pub fn writer_of(doc: &Value) -> Option<&str> {
    doc.get("writer").and_then(Value::as_str)
}

fn rows_mut(doc: &mut Value) -> Result<&mut Vec<Value>, String> {
    let obj = doc.as_object_mut().ok_or("the feed is not a JSON object")?;
    let rows = obj.entry("rows").or_insert_with(|| json!([]));
    if !rows.is_array() {
        *rows = json!([]);
    }
    Ok(rows.as_array_mut().expect("just made an array"))
}

fn find(rows: &[Value], key: &str) -> Option<usize> {
    rows.iter()
        .position(|r| r.get("key").and_then(Value::as_str) == Some(key))
}

fn set_str(row: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(v) = v {
        row.insert(key.into(), Value::String(v.clone()));
    }
}

/// Close an open pause at `now`: `started` moves on by the span, which is
/// added to `paused_s`; `spawned` keeps the true start.
fn close_pause(row: &mut Map<String, Value>, now: u64) {
    let Some(since) = ms_of(row, "paused_since") else {
        return;
    };
    row.remove("paused_since");
    // A killed row's pause is its off span: closed, it runs.
    row.remove("killed");
    let span = now.saturating_sub(since);
    if let Some(started) = ms_of(row, "started") {
        if !row.contains_key("spawned") {
            row.insert("spawned".into(), Value::String(stamp(started)));
        }
        row.insert("started".into(), Value::String(stamp(started + span)));
    }
    // The latest figure's moment stands on the same clock:
    // the pane counts the work since it, which the pause was not.
    if let Some(at) = ms_of(row, "left_at") {
        row.insert("left_at".into(), Value::String(stamp(at + span)));
    }
    let total = u64_of(row, "paused_s").unwrap_or(0) + span / 1000;
    row.insert("paused_s".into(), json!(total));
}

/// `start <task> --agent <id>` on a landed row whose worker is `id`: the
/// worker was woken (the user typed to it from its page, and
/// the manager passed it on), so its row is Running again. The span since
/// it landed is a pause, not work: its clock carries on from where it
/// stopped, and the landing, review and end go.
fn reopen(row: &mut Map<String, Value>, key: &str, agent: &str, f: &Flags, now: u64) -> String {
    let ended = ms_of(row, "ended").unwrap_or(now);
    let off = now.saturating_sub(ended);
    if !row.contains_key("paused_since") {
        row.insert("paused_since".into(), json!(stamp(ended)));
    }
    close_pause(row, now);
    for k in ["ended", "landing", "review"] {
        row.remove(k);
    }
    row.insert("stage".into(), json!("running"));
    if let Some(eta) = f.eta_s {
        row.insert("eta_s".into(), json!(eta));
        row.insert("eta_at".into(), json!(stamp(now)));
    }
    set_str(row, "note", &f.note);
    format!(
        "reopened {key}: its worker {agent} runs again; the {} since it landed is left out \
         of its time",
        feed::fmt_span((off / 1000) as i64)
    )
}

/// Giverny, WSL or this manager session went off and every worker of the
/// session went with it (told by [`crate::away`]): each Running
/// row not paused already has its clock stopped where the session last
/// showed life, and reads `killed`. That is the latest of `last_ms` (its
/// last hook call), a Running row's worker's transcript last written
/// (`written`, by agent id) and a Running row's last measured run: its
/// workers died with it, so the latest of them is when it went off. The
/// stop is an open pause, so the off span and the time until a revive are
/// left out of the row's running time, its countdown and the history, as a
/// pause is; only its manager closes it, with `resume` (or `start --agent`,
/// `land`). Returns the keys killed.
pub fn kill_running(
    doc: &mut Value,
    last_ms: u64,
    now: u64,
    written: impl Fn(&str) -> Option<u64>,
) -> Vec<String> {
    let Ok(rows) = rows_mut(doc) else {
        return Vec::new();
    };
    let live = |r: &Map<String, Value>| {
        stage_of(r) == Some(feed::Stage::Running) && !r.contains_key("paused_since")
    };
    let off = rows
        .iter()
        .filter_map(Value::as_object)
        .filter(|r| live(r))
        .flat_map(|r| {
            let ran = r
                .get("usage")
                .and_then(Value::as_object)
                .and_then(|u| ms_of(u, "last_at"));
            let wrote = r.get("agent_id").and_then(Value::as_str).and_then(&written);
            [ran, wrote]
        })
        .flatten()
        .fold(last_ms, u64::max)
        .min(now);
    let mut killed = Vec::new();
    for row in rows.iter_mut().filter_map(Value::as_object_mut) {
        if !live(row) {
            continue;
        }
        let since = off.max(ms_of(row, "started").unwrap_or(0)).min(now);
        // An open wait ends where the clock stops: counted once, as off.
        close_wait(row, since);
        row.insert("paused_since".into(), json!(stamp(since)));
        row.entry("paused_s").or_insert(json!(0));
        row.insert("killed".into(), json!(stamp(now)));
        // Kept past the revive: a worker silent since died then.
        row.insert("went_off".into(), json!(stamp(since)));
        if let Some(k) = row.get("key").and_then(Value::as_str) {
            killed.push(k.to_string());
        }
    }
    killed
}

/// [`kill_running`] on `session`'s feed `doc`, its workers' transcripts
/// looked up under the session's ids (now and before a re-id) in the Claude
/// config dir this process runs in.
pub fn kill_running_in(doc: &mut Value, session: &str, last_ms: u64, now: u64) -> Vec<String> {
    let mut sessions = vec![session.to_string()];
    sessions.extend(
        doc.get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from),
    );
    kill_running(doc, last_ms, now, |agent| {
        last_written(None, &sessions, agent)
    })
}

/// Is this `--why` the worker saying it is waiting rather than working?
fn is_wait(why: Option<&str>) -> bool {
    why.is_some_and(|w| {
        let w = w.trim().to_ascii_lowercase();
        w == "wait" || w == "waiting" || w == "blocked"
    })
}

/// Close an open waiting span at `now`: its length goes to `wait_s`, which
/// the history takes off the task's working time. The pane's
/// clock is not moved: a wait is still wall time.
fn close_wait(row: &mut Map<String, Value>, now: u64) {
    let Some(since) = ms_of(row, "waiting_since") else {
        return;
    };
    row.remove("waiting_since");
    let total = u64_of(row, "wait_s").unwrap_or(0) + now.saturating_sub(since) / 1000;
    row.insert("wait_s".into(), json!(total));
}

/// Working time so far on a Running row, in seconds: since `started`
/// (already moved on by closed pauses) up to an open pause, less its closed
/// and open waiting spans and what the machine slept through.
pub(crate) fn worked_s(row: &Map<String, Value>, now: u64, sleeps: &[sleeps::Span]) -> u64 {
    let Some(started) = ms_of(row, "started") else {
        return 0;
    };
    let upto = ms_of(row, "paused_since").unwrap_or(now);
    let open_wait = ms_of(row, "waiting_since").map_or(0, |w| upto.saturating_sub(w) / 1000);
    let slept = sleeps::slept_ms(sleeps, started, upto) / 1000;
    (upto.saturating_sub(started) / 1000)
        .saturating_sub(u64_of(row, "wait_s").unwrap_or(0) + open_wait + slept)
}

/// A new estimate on a row drops what a row from an older version kept
/// beside its corrected `eta_s`: the raw guess and where the correction came
/// from. `eta_s` is the guess now.
fn drop_old_guess(row: &mut Map<String, Value>, f: &Flags) {
    if f.eta_s.is_some() {
        row.remove("eta_guess_s");
        row.remove("eta_basis");
    }
}

/// A new guess on a row keeps its giver's gut figure (`eta_gut_s`: `--gut`,
/// else the guess itself), the factor it had been shown
/// (`eta_told_x`) and the level of the line printed with it
/// (`guess_level`). Called only where the command gave a figure.
fn told_guess(row: &mut Map<String, Value>, f: &Flags) {
    if let Some(gut) = f.gut_s.or(f.eta_s) {
        row.insert("eta_gut_s".into(), json!(gut));
    }
    row.insert("eta_told_x".into(), json!(f.told_x.unwrap_or(1.0)));
    if let Some(level) = &f.guess_level {
        row.insert("guess_level".into(), json!(level));
    }
}

/// `start <task> --agent <worker>` on a worker already running another
/// task: the dispatcher has handed it the next one, so the
/// worker's earlier Running rows land now, each with its own measured span,
/// and the pane gives the new task its own clock and its own tokens. A row
/// started less than [`feed::LATER_TASK_MS`] before is not an earlier task
/// but one of a batch the worker was spawned with (`Work #144 #145`), and
/// keeps running. Returns the keys landed.
///
/// The worker's rows are those carrying its `agent_id`, and — since a
/// dispatcher may `start` a task before the spawn tells it the id — those
/// with no `agent_id` whose key its spawn description (`desc`) names, the
/// join the pane makes. A landed row loses its `lease`: the caller gives it
/// back to the ledger.
fn hand_off(
    rows: &mut [Value],
    new: usize,
    agent: &str,
    desc: Option<&str>,
    now: u64,
) -> Vec<String> {
    let mut landed = Vec::new();
    for (i, row) in rows.iter_mut().enumerate() {
        let Some(row) = row.as_object_mut() else {
            continue;
        };
        if i == new || !earlier_task(row, agent, desc, now) {
            continue;
        }
        close_pause(row, now);
        close_wait(row, now);
        row.insert("stage".into(), json!("done"));
        row.insert("ended".into(), json!(stamp(now)));
        row.entry("landing").or_insert(json!("Done"));
        row.entry("agent_id").or_insert(json!(agent));
        row.remove("lease");
        if let Some(k) = row.get("key").and_then(Value::as_str) {
            landed.push(k.to_string());
        }
    }
    landed
}

/// Whether `row` is an earlier task of worker `agent` that handing it a
/// new one lands ([`hand_off`]): Running, its own (by `agent_id`, or with
/// none by its spawn description `desc`), and started at least
/// [`feed::LATER_TASK_MS`] before `now`.
fn earlier_task(row: &Map<String, Value>, agent: &str, desc: Option<&str>, now: u64) -> bool {
    let key = row.get("key").and_then(Value::as_str).unwrap_or("");
    let theirs = match row.get("agent_id").and_then(Value::as_str) {
        Some(a) => a == agent,
        None => desc.is_some_and(|d| feed::names_key(d, key)),
    };
    theirs
        && stage_of(row) == Some(feed::Stage::Running)
        && ms_of(row, "started").is_some_and(|s| s.saturating_add(feed::LATER_TASK_MS) <= now)
}

/// The keys handing `task` to `agent` would land ([`hand_off`]), before
/// it is done: what `start --agent` checks for anything still running.
fn handed_keys(doc: &Value, task: &str, agent: &str, desc: Option<&str>, now: u64) -> Vec<String> {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|r| r.get("key").and_then(Value::as_str) != Some(task))
        .filter(|r| earlier_task(r, agent, desc, now))
        .filter_map(|r| r.get("key").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Whether a `start --agent` could hand off a row the agent id alone does
/// not find: a Running row with no `agent_id`, old enough to be an earlier
/// task. Only then is the worker's spawn description looked up.
fn needs_description(doc: &Value, now: u64) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|r| {
            !r.contains_key("agent_id")
                && stage_of(r) == Some(feed::Stage::Running)
                && ms_of(r, "started").is_some_and(|s| s.saturating_add(feed::LATER_TASK_MS) <= now)
        })
}

/// `session`'s `subagents/` dir, under the Claude config dir it runs in.
fn session_subagents(cfg: Option<&Path>, session: &str) -> Option<PathBuf> {
    let cfg = cfg
        .map(Path::to_path_buf)
        .or_else(continuation::config_dir)?;
    crate::subagents::subagents_dir(&cfg, session)
}

/// The transcript of subagent `agent`, read whole: under the first of
/// `sessions` (the conversation's ids, now and before a re-id) that has it.
fn worker_log(cfg: Option<&Path>, sessions: &[String], agent: &str) -> Option<WorkerLog> {
    sessions.iter().find_map(|session| {
        let dir = session_subagents(cfg, session)?;
        let path = crate::subagents::agent_transcript(&dir, agent);
        if !path.exists() {
            return None;
        }
        let mut log = WorkerLog::new(path);
        log.poll().then_some(log)
    })
}

/// Freeze what each row that just landed spent, when its worker held other
/// tasks before or after it: `task_tokens`, its worker's turns from the
/// row's start (the worker's first turn, for its first task) to its landing.
/// The pane draws that figure for the Done row from then on, whatever the
/// worker does next. A worker that held one task keeps its own count, and
/// a row whose transcript cannot be read is left for the pane to split.
/// A worker's task that landed before it was handed the next (`land`, then
/// `start <next> --agent <worker>`) is frozen at that hand-off: `handed` is
/// that worker.
fn freeze_tokens(
    doc: &mut Value,
    before: &[String],
    handed: Option<&str>,
    log: impl Fn(&str) -> Option<WorkerLog>,
) {
    let Some(rows) = doc.get_mut("rows").and_then(Value::as_array_mut) else {
        return;
    };
    let began = |r: &Map<String, Value>| ms_of(r, "spawned").or_else(|| ms_of(r, "started"));
    let starts: Vec<(String, u64, Option<u64>)> = rows
        .iter()
        .filter_map(Value::as_object)
        .filter(|r| stage_of(r) != Some(feed::Stage::Planned))
        .filter_map(|r| {
            let agent = r.get("agent_id")?.as_str()?.to_string();
            Some((agent, began(r)?, ms_of(r, "ended")))
        })
        .collect();
    for row in rows.iter_mut().filter_map(Value::as_object_mut) {
        let key = row.get("key").and_then(Value::as_str).unwrap_or("");
        let agent_now = handed.is_some() && row.get("agent_id").and_then(Value::as_str) == handed;
        if stage_of(row) != Some(feed::Stage::Done)
            || (before.iter().any(|k| k == key) && !agent_now)
            || row.contains_key("task_tokens")
        {
            continue;
        }
        let (Some(agent), Some(start), Some(end)) = (
            row.get("agent_id").and_then(Value::as_str),
            began(row),
            ms_of(row, "ended"),
        ) else {
            continue;
        };
        let other = |far: &dyn Fn(u64, Option<u64>) -> bool| {
            starts.iter().any(|(a, s, e)| a == agent && far(*s, *e))
        };
        // Another of its tasks, before or after: one begun well apart, or
        // one that had landed before the other began, however short.
        let earlier = other(&|s, e| {
            s.saturating_add(feed::LATER_TASK_MS) <= start
                || (s < start && e.is_some_and(|e| e <= start))
        });
        let later = other(&|s, _| {
            start.saturating_add(feed::LATER_TASK_MS) <= s || (start < s && end <= s)
        });
        if !earlier && !later {
            continue;
        }
        let Some(w) = log(agent) else { continue };
        let from = if earlier { start } else { 0 };
        row.insert("task_tokens".into(), json!(w.added(from, Some(end))));
    }
}

/// Whether any Running row has no worker: only then can a spawn have
/// come for it that the feed does not know of yet.
fn has_unworkered(doc: &Value) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|r| !r.contains_key("agent_id") && stage_of(r) == Some(feed::Stage::Running))
}

/// A subagent of this session as its spawn left it: its id, the Agent
/// tool's `description`, and when its transcript was first written.
#[derive(Debug, Clone)]
struct Spawn {
    agent: String,
    description: String,
    at_ms: Option<u64>,
}

/// Every subagent of the conversation's `sessions` that has a spawn
/// description.
fn spawns(cfg: Option<&Path>, sessions: &[String]) -> Vec<Spawn> {
    let mut out: Vec<Spawn> = Vec::new();
    for session in sessions {
        let Some(dir) = session_subagents(cfg, session) else {
            continue;
        };
        for agent in crate::subagents::list_agent_ids(&dir) {
            if out.iter().any(|s| s.agent == agent) {
                continue;
            }
            let Some(description) = crate::subagents::read_meta(&dir, &agent).description else {
                continue;
            };
            let at_ms =
                worker_log::first_written_ms(&crate::subagents::agent_transcript(&dir, &agent));
            out.push(Spawn {
                agent,
                description,
                at_ms,
            });
        }
    }
    out
}

/// Give each Running row with no worker the one worker spawned for it: the
/// subagent whose spawn description names its key and that first wrote no
/// earlier than [`feed::HANDOFF_WINDOW_MS`] before the row started. The
/// hook does the same at the worker's first tool call; done here too, the
/// row is that worker's before any message to another worker that names it
/// can be read as handing it over. Returns the keys stamped.
fn stamp_spawns(doc: &mut Value, spawns: &[Spawn]) -> Vec<String> {
    let Some(rows) = doc.get_mut("rows").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut stamped = Vec::new();
    for r in rows.iter_mut().filter_map(Value::as_object_mut) {
        if r.contains_key("agent_id") || stage_of(r) != Some(feed::Stage::Running) {
            continue;
        }
        let Some(key) = r
            .get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
        else {
            continue;
        };
        let started = ms_of(r, "started");
        let theirs: Vec<&Spawn> = spawns
            .iter()
            .filter(|s| feed::names_key(&s.description, key))
            .filter(|s| match (s.at_ms, started) {
                (Some(at), Some(st)) => at.saturating_add(feed::HANDOFF_WINDOW_MS) >= st,
                _ => true,
            })
            .collect();
        if let [one] = theirs.as_slice() {
            stamped.push(key.to_string());
            r.insert("agent_id".into(), json!(one.agent));
        }
    }
    stamped
}

/// Whether any Running row has a worker: only then can one have been
/// resumed by a fresh worker ([`relink_resumed`]).
fn has_worked(doc: &Value) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|r| r.contains_key("agent_id") && stage_of(r) == Some(feed::Stage::Running))
}

/// Move each Running row whose worker has finished to the worker spawned
/// after it for the same task: the dispatcher resumed the task with a fresh
/// worker and never ran `start <task> --agent <id>`. The new
/// worker is the newest spawn whose description names the key, first
/// written after the old worker last wrote, that no row names by id; the
/// old one must have `finished` (its completion notice is its last word).
/// `last_write` is when a worker's transcript was last written. Returns the
/// keys moved.
fn relink_resumed(
    doc: &mut Value,
    spawns: &[Spawn],
    last_write: impl Fn(&str) -> Option<u64>,
    finished: impl Fn(&str, Option<u64>) -> bool,
) -> Vec<String> {
    let Some(rows) = doc.get_mut("rows").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let held: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("agent_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut moved = Vec::new();
    for r in rows.iter_mut().filter_map(Value::as_object_mut) {
        if stage_of(r) != Some(feed::Stage::Running) {
            continue;
        }
        let (Some(key), Some(old)) = (
            r.get("key")
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty()),
            r.get("agent_id").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(quiet) = last_write(old) else {
            continue;
        };
        let new = spawns
            .iter()
            .filter(|s| !held.contains(&s.agent) && feed::names_key(&s.description, key))
            .filter(|s| s.at_ms.is_some_and(|at| at > quiet))
            .max_by_key(|s| s.at_ms);
        let Some(new) = new else { continue };
        if !finished(old, Some(quiet)) {
            continue;
        }
        moved.push(key.to_string());
        r.insert("agent_id".into(), json!(new.agent));
    }
    moved
}

/// When subagent `agent`'s transcript was last written, under the first of
/// `sessions` that has it.
fn last_written(cfg: Option<&Path>, sessions: &[String], agent: &str) -> Option<u64> {
    sessions.iter().find_map(|session| {
        let dir = session_subagents(cfg, session)?;
        let t = std::fs::metadata(crate::subagents::agent_transcript(&dir, agent))
            .and_then(|m| m.modified())
            .ok()?;
        Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
    })
}

/// `start <task>` with no `--agent`: the workers of this pass that are
/// idle now — every row they hold has landed, the last within
/// [`IDLE_HINT_MS`] — newest landing first, with that row's key. One of them
/// may be about to be handed `task`.
fn idle_workers(doc: &Value, task: &str, now: u64) -> Vec<(String, String)> {
    let rows: Vec<&Map<String, Value>> = doc
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect();
    let mut idle: Vec<(u64, String, String)> = Vec::new();
    for r in &rows {
        let Some(a) = r.get("agent_id").and_then(Value::as_str) else {
            continue;
        };
        if a.is_empty() || idle.iter().any(|(_, x, _)| x == a) {
            continue;
        }
        let theirs: Vec<&&Map<String, Value>> = rows
            .iter()
            .filter(|r| r.get("agent_id").and_then(Value::as_str) == Some(a))
            .filter(|r| r.get("key").and_then(Value::as_str) != Some(task))
            .collect();
        if theirs
            .iter()
            .any(|r| stage_of(r) != Some(feed::Stage::Done))
        {
            continue;
        }
        let last = theirs
            .iter()
            .filter_map(|r| Some((ms_of(r, "ended")?, r.get("key")?.as_str()?)))
            .max_by_key(|(e, _)| *e);
        if let Some((ended, key)) = last
            && now.saturating_sub(ended) <= IDLE_HINT_MS
        {
            idle.push((ended, a.to_string(), key.to_string()));
        }
    }
    idle.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
    idle.into_iter().map(|(_, a, k)| (a, k)).collect()
}

/// How lately a worker's last task must have landed for `start` with no
/// `--agent` to name it as idle.
const IDLE_HINT_MS: u64 = 30 * 60 * 1000;

/// The most context a worker may carry and still be handed a next task.
/// Every turn of a reused worker re-reads its whole context,
/// so a heavy one makes the next task slow and dear, and its old context
/// crowds the new one: above this a fresh worker with a full brief is
/// better. `start --agent` refuses a worker over it unless `--heavy-ok`, and
/// a plain `start` never suggests one.
pub const REUSE_MAX_TOKENS: u64 = 100_000;

/// Whether a worker carrying `context` is too heavy to reuse. An unknown
/// context (no transcript to read) is not.
fn too_heavy(context: Option<u64>) -> bool {
    context.is_some_and(|n| n > REUSE_MAX_TOKENS)
}

/// The context worker `agent` carries now, as the panel's TOKENS column and
/// the status line count it ([`crate::tokens::tokens_of`]): its transcript's
/// last API response, under the first of `sessions` that has it.
fn worker_context(cfg: Option<&Path>, sessions: &[String], agent: &str) -> Option<u64> {
    sessions.iter().find_map(|session| {
        let dir = session_subagents(cfg, session)?;
        crate::tokens::tokens_of(&crate::subagents::agent_transcript(&dir, agent))
    })
}

/// An idle worker for [`idle_hint`]: its id, the key of the task it landed
/// last, and the context it carries.
type Idle = (String, String, Option<u64>);

/// The lines `start <task>` with no `--agent` adds when workers of this
/// manager session are idle: the newest-landed one under
/// [`REUSE_MAX_TOKENS`] to hand it over with `--agent`, so its tokens are its
/// own, and the ones over it named as better left alone.
fn idle_hint(task: &str, idle: &[Idle]) -> Option<String> {
    let ctx = |c: &Option<u64>| match c {
        Some(n) => format!(", carrying {} context", crate::tokens::fmt_tokens(*n)),
        None => String::new(),
    };
    let limit = crate::tokens::fmt_tokens(REUSE_MAX_TOKENS);
    let (heavy, light): (Vec<&Idle>, Vec<&Idle>) = idle.iter().partition(|w| too_heavy(w.2));
    let mut lines = Vec::new();
    if let Some((agent, key, c)) = light.first() {
        let more = match light.len() {
            1 => String::new(),
            n => format!(" ({} more idle under {limit})", n - 1),
        };
        lines.push(format!(
            "  {agent} is idle since {key} landed{c}{more}: if {task} is in code it has read \
             or follows on from {key}, start it with `--agent {agent}` so {task} counts its \
             own tokens; otherwise spawn a fresh worker",
            c = ctx(c)
        ));
    }
    if !heavy.is_empty() {
        let names: Vec<String> = heavy
            .iter()
            .map(|(a, k, c)| format!("{a} (since {k} landed{})", ctx(c)))
            .collect();
        let (who, verb) = match names.len() {
            1 => (names[0].clone(), "is"),
            _ => (names.join(", "), "are"),
        };
        lines.push(format!(
            "  {who} {verb} idle but over the {limit} reuse limit: every turn of a reused \
             worker re-reads its whole context, so spawn a fresh worker with a full brief \
             for {task}"
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `start <task> --agent <worker>`'s guard: refuse a worker over
/// [`REUSE_MAX_TOKENS`] unless `--heavy-ok`, saying how heavy it is; with
/// `--heavy-ok`, the note to add. A worker already holding `task`, or
/// spawned for it, is not being reused (the caller checks), and one whose
/// context cannot be read is let through.
fn heavy_guard(
    task: &str,
    agent: &str,
    context: Option<u64>,
    heavy_ok: bool,
) -> Result<Option<String>, String> {
    let Some(n) = context.filter(|_| too_heavy(context)) else {
        return Ok(None);
    };
    let (size, limit) = (
        crate::tokens::fmt_tokens(n),
        crate::tokens::fmt_tokens(REUSE_MAX_TOKENS),
    );
    if heavy_ok {
        return Ok(Some(format!(
            "  {agent} carries {size} context, over the {limit} reuse limit: handed {task} \
             anyway (--heavy-ok)"
        )));
    }
    Err(format!(
        "{agent} carries {size} context, over the {limit} reuse limit: every turn of a \
         reused worker re-reads its whole context, so {task} would run slow and dear with \
         that old context crowding it. Spawn a fresh worker with a full brief and \
         `start {task}` without --agent; if {task} really needs what {agent} has read, pass \
         --heavy-ok"
    ))
}

/// The spawn description of `session`'s subagent `agent`, from its
/// `agent-<id>.meta.json` under the Claude config dir the session runs in.
fn agent_description(cfg: Option<&Path>, sessions: &[String], agent: &str) -> Option<String> {
    sessions.iter().find_map(|session| {
        let dir = session_subagents(cfg, session)?;
        crate::subagents::read_meta(&dir, agent).description
    })
}

/// Apply one command to a feed document at `now` (epoch ms). Returns the
/// line to print. `Show`, `Path`, `Clear` and `ClearDone` are the caller's.
pub fn apply(
    doc: &mut Value,
    cmd: &Cmd,
    f: &Flags,
    now: u64,
    sleeps: &[sleeps::Span],
) -> Result<String, String> {
    let rows = rows_mut(doc)?;
    let key = match cmd {
        Cmd::Plan(k)
        | Cmd::Start(k)
        | Cmd::Eta(k, _)
        | Cmd::Land(k)
        | Cmd::Pause(k)
        | Cmd::Resume(k)
        | Cmd::Drop(k)
        | Cmd::Hold(k)
        | Cmd::Unhold(k) => k.clone(),
        Cmd::Name(_)
        | Cmd::Show
        | Cmd::Path
        | Cmd::Clear
        | Cmd::ClearDone
        | Cmd::Claim(_)
        | Cmd::Release(_)
        | Cmd::Resources
        | Cmd::Accuracy
        | Cmd::Watch
        | Cmd::Run(_)
        | Cmd::Ask(..)
        | Cmd::Reply(..) => {
            return Ok(String::new());
        }
    };
    let at = find(rows, &key);
    let stage = at.and_then(|i| rows[i].as_object().and_then(stage_of));
    let missing =
        || format!("no task `{key}` in this manager session (`giverny manage show` lists them)");

    match cmd {
        Cmd::Plan(_) => {
            if let Some(s) = stage.filter(|s| *s != feed::Stage::Planned) {
                return Err(format!(
                    "`{key}` is already {}; `giverny manage eta` re-estimates it",
                    stage_word(s)
                ));
            }
            // A task waits only on one that is in the plan: a typo would
            // be a silent wait.
            let after = f.after.as_deref().filter(|a| !a.is_empty());
            if let Some(a) = after {
                if a == key {
                    return Err(format!("`{key}` cannot wait for itself"));
                }
                if find(rows, a).is_none() {
                    return Err(format!(
                        "no task `{a}` in this manager session to wait for: plan it first"
                    ));
                }
            }
            let i = at.unwrap_or_else(|| {
                rows.push(json!({ "key": key }));
                rows.len() - 1
            });
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            row.insert("stage".into(), json!("planned"));
            // When it joined the plan: the pane flags a Next up row left
            // idle a while with nothing recorded to wait on.
            row.entry("planned").or_insert(json!(stamp(now)));
            match (f.after.as_deref(), after) {
                (_, Some(a)) => {
                    row.insert("after".into(), json!(a));
                }
                (Some(_), None) => {
                    row.remove("after");
                }
                _ => {}
            }
            row.insert("eta_s".into(), json!(f.eta_s.unwrap_or(0)));
            told_guess(row, f);
            drop_old_guess(row, f);
            set_str(row, "title", &f.title);
            set_str(row, "note", &f.note);
            set_str(row, "brief", &f.brief);
            set_str(row, "repo", &f.repo);
            Ok(format!("planned {key}"))
        }
        Cmd::Start(_) => {
            if stage == Some(feed::Stage::Done) {
                let i = at.ok_or_else(missing)?;
                let row = rows[i].as_object_mut().ok_or("row is not an object")?;
                let held_by = row.get("agent_id").and_then(Value::as_str);
                return match f.agent.as_deref() {
                    Some(agent) if held_by == Some(agent) => Ok(reopen(row, &key, agent, f, now)),
                    _ => Err(format!(
                        "`{key}` has landed already. Its worker woken again (SendMessage) \
                         reopens it with `start {key} --agent <that worker's id>`"
                    )),
                };
            }
            let i = at.unwrap_or_else(|| {
                rows.push(json!({ "key": key }));
                rows.len() - 1
            });
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            let msg = if stage == Some(feed::Stage::Running) {
                close_pause(row, now);
                format!("{key} is running already (since {})", {
                    ms_of(row, "started").map(stamp).unwrap_or_default()
                })
            } else {
                row.insert("stage".into(), json!("running"));
                row.insert("started".into(), json!(stamp(now)));
                for k in [
                    "ended",
                    "paused_since",
                    "paused_s",
                    "killed",
                    "went_off",
                    "spawned",
                    "waiting_since",
                    "wait_s",
                    "reestimate_asked",
                    "track_told",
                    "track_x",
                    "reest_open_at",
                    "eta_at",
                    "left_s",
                    "left_at",
                    "deadline_asked",
                    "overdue_asked",
                    "progress_more",
                    "progress_overdue",
                    "progress_told",
                    "overdue_told",
                    "after",
                    "held",
                    "held_at",
                    "unheld",
                ] {
                    row.remove(k);
                }
                format!("started {key}")
            };
            if let Some(eta) = f.eta_s {
                row.insert("eta_s".into(), json!(eta));
                // When the figure was given: the hook's near-the-end ask
                // leaves a fresh one alone a while.
                row.insert("eta_at".into(), json!(stamp(now)));
                // A whole estimate again, counted from `started`: an older
                // `eta`'s figure no longer stands.
                row.remove("left_s");
                row.remove("left_at");
                told_guess(row, f);
            }
            drop_old_guess(row, f);
            set_str(row, "repo", &f.repo);
            set_str(row, "title", &f.title);
            set_str(row, "agent_id", &f.agent);
            set_str(row, "note", &f.note);
            set_str(row, "brief", &f.brief);
            // A dispatcher's `start` owns the row: it lands it.
            row.remove("follows_worker");
            let handed = match &f.agent {
                Some(agent) => hand_off(rows, i, agent, f.agent_desc.as_deref(), now),
                None => Vec::new(),
            };
            Ok(match handed.as_slice() {
                [] => msg,
                done => format!(
                    "{msg}; {agent} handed on from {}",
                    done.join(", "),
                    agent = f.agent.as_deref().unwrap_or("")
                ),
            })
        }
        Cmd::Eta(_, left) if at.is_none() => {
            // A worker spawned outside a manager session has no row
            // for `eta` to re-estimate, and an error would teach nothing:
            // start one now, with what is left as its estimate. The time
            // the worker spent before this is not known, so the clock starts
            // here; and the figure is taken as given, as every figure is.
            let mut sf = f.clone();
            sf.eta_s = Some(*left);
            apply(doc, &Cmd::Start(key.clone()), &sf, now, sleeps)?;
            let rows = rows_mut(doc)?;
            if let Some(row) = find(rows, &key).and_then(|i| rows[i].as_object_mut()) {
                // Started from `eta`, by a dispatcher that forgot `start`
                // (or a worker of an older version): none may
                // ever land it, so the pane ends it with its worker.
                row.insert("follows_worker".into(), json!(true));
                if is_wait(f.why.as_deref()) {
                    row.insert("waiting_since".into(), json!(stamp(now)));
                }
            }
            Ok(format!(
                "{key}: no row in this manager session, so started it now with ~{} left \
                 (as given; `giverny manage start {key} --eta <min> --agent <id>` \
                 before the spawn gives a row its whole time)",
                feed::fmt_span(*left as i64)
            ))
        }
        Cmd::Eta(_, left) => {
            let i = at.ok_or_else(missing)?;
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            // The figure before, for the progress check.
            let before = figure_left(row, now, sleeps);
            let first = stage == Some(feed::Stage::Running) && !row.contains_key("reest_s");
            // The figure a worker settles on after its `eta` reply asked for
            // it: with `--gut`, soon after, it takes the
            // asked-about figure's place as the scored re-estimate, so one
            // estimate does not count twice in the history.
            let settles = stage == Some(feed::Stage::Running)
                && !first
                && f.gut_s.is_some()
                && ms_of(row, "reest_open_at").is_some_and(|t| now.saturating_sub(t) < SETTLE_MS);
            let replaced = settles.then(|| u64_of(row, "reest_s")).flatten();
            if settles {
                row.remove("reest_s");
                row.remove("reest_open_at");
            }
            if stage == Some(feed::Stage::Running) && !row.contains_key("reest_s") {
                // The worker's first re-estimate: kept, with
                // when it was made in working time, to be scored at landing.
                row.insert("reest_s".into(), json!(left));
                row.insert("reest_at_s".into(), json!(worked_s(row, now, sleeps)));
                // Its gut figure: `--gut`, else the figure is
                // the gut figure. The history learns from it.
                row.insert("reest_gut_s".into(), json!(f.gut_s.unwrap_or(*left)));
                // With the factor the hook had told the worker before this
                // call, if any, for the record.
                let shown = row.get("track_x").and_then(Value::as_f64).unwrap_or(1.0);
                row.insert("reest_told_x".into(), json!(shown));
            }
            if let (Some(x), Some(feed::Stage::Running)) = (f.reest_x, stage) {
                // This reply tells the worker its line and asks for the
                // figure it settles on: the row remembers the
                // factor told, and the hook's five-minute ask is not needed.
                row.insert("track_x".into(), json!(x));
                row.entry("reestimate_asked").or_insert(json!(stamp(now)));
                if first {
                    row.insert("reest_open_at".into(), json!(stamp(now)));
                }
            }
            if stage == Some(feed::Stage::Planned) {
                // A dispatcher's new guess on a row not started yet.
                row.insert("eta_gut_s".into(), json!(f.gut_s.unwrap_or(*left)));
                row.insert("eta_told_x".into(), json!(f.told_x.unwrap_or(1.0)));
            }
            let eta = match stage {
                Some(feed::Stage::Running) => {
                    let started = ms_of(row, "started").unwrap_or(now);
                    let upto = ms_of(row, "paused_since").unwrap_or(now);
                    upto.saturating_sub(started) / 1000 + left
                }
                Some(feed::Stage::Done) => return Err(format!("`{key}` has landed already")),
                _ => *left,
            };
            if stage == Some(feed::Stage::Running)
                && !row.contains_key("eta_first_s")
                && let Some(first) = u64_of(row, "eta_s")
            {
                row.insert("eta_first_s".into(), json!(first));
            }
            row.insert("eta_s".into(), json!(eta));
            if stage == Some(feed::Stage::Running) {
                row.insert("eta_at".into(), json!(stamp(now)));
                // The figure itself, and where the row's clock stood when it
                // was given: the pane and `show` count down from
                // it by the work since. `eta_s` above is wall time to here
                // plus the figure, so `eta_s` less the work so far took every
                // hold, sleep and error span before this off twice.
                row.insert("left_s".into(), json!(left));
                let at = ms_of(row, "paused_since").unwrap_or(now);
                row.insert("left_at".into(), json!(stamp(at)));
                // A finish moved much later is put before the manager.
                if !row.contains_key("paused_since") {
                    crate::manage_progress::more_time(row, before, *left, now);
                }
            }
            set_str(row, "note", &f.note);
            if stage == Some(feed::Stage::Running) {
                if is_wait(f.why.as_deref()) {
                    if !row.contains_key("waiting_since") && !row.contains_key("paused_since") {
                        row.insert("waiting_since".into(), json!(stamp(now)));
                    }
                } else {
                    close_wait(row, now);
                }
            }
            let mut msg = format!("{key}: ~{} left", feed::fmt_span(*left as i64));
            if let (Some(old), Some(gut)) = (replaced, f.gut_s) {
                msg.push_str(&format!(
                    " (gut {}), in place of the {} just given",
                    feed::fmt_span(gut as i64),
                    feed::fmt_span(old as i64)
                ));
            }
            // `--agent` moves the row to that worker, as `start --agent` does.
            let Some(agent) = f.agent.as_deref() else {
                return Ok(msg);
            };
            if row.get("agent_id").and_then(Value::as_str) == Some(agent) {
                return Ok(msg);
            }
            row.insert("agent_id".into(), json!(agent));
            let handed = hand_off(rows, i, agent, f.agent_desc.as_deref(), now);
            Ok(match handed.as_slice() {
                [] => format!("{msg}; now held by {agent}"),
                done => format!(
                    "{msg}; now held by {agent}, handed on from {}",
                    done.join(", ")
                ),
            })
        }
        Cmd::Land(_) => {
            let i = at.ok_or_else(missing)?;
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            close_pause(row, now);
            close_wait(row, now);
            if !row.contains_key("started") {
                // Landed without ever being started: its span is unknown, so
                // it gets no delta rather than a made-up one.
                row.remove("eta_s");
            }
            let outcome = f
                .outcome
                .clone()
                .unwrap_or_else(|| if f.review.is_some() { "Review" } else { "Done" }.to_string());
            let who = f
                .review
                .as_deref()
                .and_then(|r| r.split(" — ").next())
                .map(str::trim)
                .filter(|w| !w.is_empty() && f.outcome.is_none());
            let mut landing = match who {
                Some(w) => format!("{outcome} — {w}"),
                None => outcome,
            };
            if let Some(n) = f.note.as_deref().filter(|n| !n.trim().is_empty()) {
                landing = format!("{landing} — {n}");
            }
            row.insert("stage".into(), json!("done"));
            row.insert("ended".into(), json!(stamp(now)));
            row.insert("landing".into(), json!(landing));
            // `run_in` gives the lease back to the ledger.
            row.remove("lease");
            set_str(row, "review", &f.review);
            set_str(row, "agent_id", &f.agent);
            Ok(format!("landed {key}: {landing}"))
        }
        Cmd::Pause(_) => {
            let i = at.ok_or_else(missing)?;
            if stage != Some(feed::Stage::Running) {
                return Err(format!("`{key}` is not running"));
            }
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            // A pause supersedes a wait: the span is counted once, as paused.
            close_wait(row, now);
            if !row.contains_key("paused_since") {
                row.insert("paused_since".into(), json!(stamp(now)));
                row.entry("paused_s").or_insert(json!(0));
            }
            // A killed row paused is set aside: its clock stays stopped
            // where it went off, and it reads paused.
            row.remove("killed");
            set_str(row, "note", &f.note);
            Ok(format!("paused {key}"))
        }
        Cmd::Resume(_) => {
            let i = at.ok_or_else(missing)?;
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            close_pause(row, now);
            set_str(row, "note", &f.note);
            Ok(format!("resumed {key}"))
        }
        Cmd::Drop(_) => {
            let i = at.ok_or_else(missing)?;
            rows.remove(i);
            Ok(format!("dropped {key}"))
        }
        Cmd::Hold(_) => {
            let i = at.ok_or_else(missing)?;
            if stage != Some(feed::Stage::Planned) {
                return Err(format!(
                    "`{key}` is not planned: only a Next up row is held (`pause` stops a running one)"
                ));
            }
            let why = f.why.as_deref().unwrap_or("").trim().to_string();
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            row.insert("held".into(), json!(why));
            row.insert("held_at".into(), json!(stamp(now)));
            row.remove("unheld");
            Ok(format!(
                "held {key}: {why} (the row says so; tell the user, and `giverny manage unhold {key}` ends it)"
            ))
        }
        Cmd::Unhold(_) => {
            let i = at.ok_or_else(missing)?;
            let row = rows[i].as_object_mut().ok_or("row is not an object")?;
            if row.remove("held").is_none() {
                return Err(format!("`{key}` is not held"));
            }
            row.remove("held_at");
            row.insert("unheld".into(), json!(stamp(now)));
            Ok(format!(
                "released the hold on {key}: start it once its lane and lease are free"
            ))
        }
        Cmd::Name(_)
        | Cmd::Show
        | Cmd::Path
        | Cmd::Clear
        | Cmd::ClearDone
        | Cmd::Claim(_)
        | Cmd::Release(_)
        | Cmd::Resources
        | Cmd::Accuracy
        | Cmd::Watch
        | Cmd::Run(_)
        | Cmd::Ask(..)
        | Cmd::Reply(..) => unreachable!(),
    }
}

/// What a Next up row waits on, as `show` says it: the dispatcher's hold,
/// the task it is planned after (until that lands), a queued lease.
fn planned_wait(r: &Map<String, Value>, rows: &[Value]) -> Option<String> {
    if let Some(why) = r.get("held").and_then(Value::as_str) {
        return Some(format!("held: {why}"));
    }
    if let Some(a) = r.get("after").and_then(Value::as_str) {
        let landed = rows
            .iter()
            .filter_map(Value::as_object)
            .find(|o| o.get("key").and_then(Value::as_str) == Some(a))
            .is_none_or(|o| stage_of(o) == Some(feed::Stage::Done));
        if !landed {
            return Some(format!("after {a}"));
        }
    }
    let lease = r.get("lease")?;
    (lease.get("state").and_then(Value::as_str) == Some("queued")).then(|| {
        match lease.get("position").and_then(Value::as_u64) {
            Some(p) => format!("lease #{p} in queue"),
            None => "lease in queue".into(),
        }
    })
}

/// The ms of `[from, upto]` the machine was awake: `show`'s working time.
/// Empty when `upto` is not after `from`.
fn work_ms(sleeps: &[sleeps::Span], from: u64, upto: u64) -> u64 {
    upto.saturating_sub(from)
        .saturating_sub(sleeps::slept_ms(sleeps, from, upto.max(from)))
}

/// A re-estimated row's latest figure: where its clock stood
/// when it was given, never before `started`, and the seconds it said were
/// left. `None` on a row never re-estimated since its `start`, and in
/// feeds from before the figure was kept.
fn last_figure(r: &Map<String, Value>) -> Option<(u64, u64)> {
    let at = ms_of(r, "left_at")?;
    let left = u64_of(r, "left_s")?;
    Some((ms_of(r, "started").map_or(at, |s| at.max(s)), left))
}

/// What a Running row's figure still has left at `now`, seconds (below
/// zero past it), as the pane counts it, and that figure itself: the
/// latest re-estimate less the work since it, else the whole estimate less
/// the work since the start. `None` with no figure.
pub(crate) fn figure_left(
    r: &Map<String, Value>,
    now: u64,
    sleeps: &[sleeps::Span],
) -> Option<(i64, u64)> {
    let upto = ms_of(r, "paused_since").unwrap_or(now);
    let (at, figure) = match last_figure(r) {
        Some(f) => f,
        None => (ms_of(r, "started")?, u64_of(r, "eta_s").filter(|e| *e > 0)?),
    };
    Some((
        figure as i64 - (work_ms(sleeps, at, upto) / 1000) as i64,
        figure,
    ))
}

/// The rows as text, in the pane's section order.
pub fn show(doc: &Value, now: u64, sleeps: &[sleeps::Span]) -> String {
    let rows = doc
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = String::new();
    if let Some(name) = doc.get("name").and_then(Value::as_str) {
        out.push_str(&format!("{name}\n"));
    }
    let named = out.len();
    for want in [
        feed::Stage::Running,
        feed::Stage::Planned,
        feed::Stage::Done,
    ] {
        for r in rows.iter().filter_map(Value::as_object) {
            if stage_of(r) != Some(want) {
                continue;
            }
            let key = r.get("key").and_then(Value::as_str).unwrap_or("?");
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let started = ms_of(r, "started");
            let eta = u64_of(r, "eta_s");
            let cell = match want {
                feed::Stage::Running => {
                    let upto = ms_of(r, "paused_since").unwrap_or(now);
                    let el = started.map(|s| work_ms(sleeps, s, upto) / 1000);
                    // The latest figure less the work since it, as the pane
                    // counts; else the whole estimate less
                    // the work since the start.
                    let left = match last_figure(r) {
                        Some((at, left)) => {
                            Some(left as i64 - (work_ms(sleeps, at, upto) / 1000) as i64)
                        }
                        None => eta.zip(el).map(|(e, el)| e as i64 - el as i64),
                    };
                    format!(
                        "{} elapsed, {}{}",
                        el.map(|e| feed::fmt_span(e as i64)).unwrap_or("?".into()),
                        left.map(|l| format!("~{} left", feed::fmt_span(l.max(0))))
                            .unwrap_or("no estimate".into()),
                        if r.contains_key("killed") {
                            ", killed"
                        } else if r.contains_key("paused_since") {
                            ", paused"
                        } else {
                            ""
                        }
                    )
                }
                feed::Stage::Planned => {
                    let est = eta
                        .map(|e| format!("~{}", feed::fmt_span(e as i64)))
                        .unwrap_or_default();
                    match planned_wait(r, &rows) {
                        Some(w) => format!("{est}, {w}"),
                        None => est,
                    }
                }
                feed::Stage::Done => {
                    let took = started
                        .zip(ms_of(r, "ended"))
                        .map(|(s, e)| work_ms(sleeps, s, e) / 1000);
                    // Against the worker's first re-estimate, else the
                    // dispatcher's figure, by the work the history counts:
                    // as the pane's Done row.
                    let delta = record_of(r, "", sleeps).and_then(|h| {
                        let reest = h.reest_s.zip(h.reest_at_s);
                        feed::done_delta(h.work_s, reest, h.estimate_s).map(|(d, _)| d)
                    });
                    let landing = r.get("landing").and_then(Value::as_str).unwrap_or("Done");
                    match took {
                        Some(t) => format!(
                            "{landing}, took {}{}",
                            feed::fmt_span(t as i64),
                            delta
                                .map(|d| format!(" {}", feed::fmt_delta(d)))
                                .unwrap_or_default()
                        ),
                        None => landing.to_string(),
                    }
                }
            };
            out.push_str(&format!(
                "{:<8} {key:<16} {cell:<32} {title}\n",
                stage_word(want)
            ));
        }
    }
    if out.len() == named {
        out.push_str("(no rows)\n");
    }
    out
}

/// A lock beside the feed, so concurrent `giverny manage` runs (a dispatcher
/// and its workers) never lose each other's rows. Taken with `create_new`;
/// one left behind by a killed run is broken after ten seconds.
pub(crate) struct Lock(PathBuf);

impl Lock {
    pub(crate) fn take(file: &Path) -> Result<Lock, String> {
        let lock = file.with_extension("json.lock");
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(Lock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(10));
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return Err(format!("{}: {e}", lock.display())),
            }
        }
        Err(format!("{} is held", lock.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `doc` to `file` atomically (a non-`.json` temp name, then rename),
/// and only when its bytes changed: the pane re-reads on a changed mtime.
pub fn write(file: &Path, doc: &Value) -> std::io::Result<()> {
    let mut data = serde_json::to_vec_pretty(doc)?;
    data.push(b'\n');
    if std::fs::read(file).is_ok_and(|old| old == data) {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::File::create(&tmp)?.write_all(&data)?;
    std::fs::rename(&tmp, file)
}

/// The file this session's manager session lives in: `<session>.json`, or an existing
/// file that names the session as an alias.
pub fn file_for(dir: &Path, session: &str) -> PathBuf {
    feed::find(dir, session)
        .map(|(p, _)| p)
        .unwrap_or_else(|| feed::feed_path(dir, session))
}

/// A session no feed names yet takes its conversation's feed under an
/// earlier id, when this writer wrote it: Claude Code re-ids a session on a
/// resume, the agents view's switch and the move into a background host
/// ([`continuation`]). The file keeps its name; the new id becomes its
/// `session` and the old one an alias, so the pane, the hook and workers
/// still holding the old id all find it.
fn adopt_continued(dir: &Path, session: &str, cfg: Option<&Path>) {
    if feed::find(dir, session).is_some() {
        return;
    }
    let Some(root) = continuation::root_of(cfg, session) else {
        return;
    };
    let mine = |d: &Value| writer_of(d) == Some(WRITER);
    let Some(file) = continuation::continued(dir, session, &root, cfg, mine) else {
        return;
    };
    let Ok(_lock) = Lock::take(&file) else {
        return;
    };
    let Some(mut doc) = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|d| d.is_object() && mine(d))
    else {
        return;
    };
    continuation::adopt(&mut doc, session);
    continuation::stamp_root(&mut doc, Some(&root));
    if let Err(e) = write(&file, &doc) {
        eprintln!("giverny manage: {}: {e}", file.display());
    }
}

/// `name`: record `session`'s name from `given` and print the full name, or
/// with none print the name it has ([`names`]: in a Giverny tab with the
/// switch on, recorded at its start; a manager named before
/// names had their own home has it on its feed), else draw it one and record
/// that (Giverny gives the name, the session picks nothing). A
/// first name, given or drawn, takes the surname of the tab's category, else
/// the repo ([`names::surname`]).
fn name(
    dir: &Path,
    session: &str,
    given: Option<&str>,
    cfg: Option<&Path>,
) -> Result<String, String> {
    // Live sessions hold their first names: `cfg`'s registry
    // and every account's.
    let live = || names::live_sessions(cfg);
    let surname = || names::surname(dir, None);
    match given {
        Some(given) => {
            let root = continuation::root_of(cfg, session);
            names::record_among(dir, session, given, root.as_deref(), surname, &live())
                .map(|(name, _)| name)
        }
        None => {
            let cfg = cfg.map(Path::to_path_buf).or_else(continuation::config_dir);
            let transcript = cfg.and_then(|c| crate::subagents::session_transcript(&c, session));
            if let Some(name) = names::find_among(dir, session, transcript.as_deref(), live)
                .or_else(|| feed::find(dir, session)?.1.name)
            {
                return Ok(name);
            }
            names::draw_and_record_among(dir, session, transcript.as_deref(), &surname(), &live())
        }
    }
}

/// Run one command against `dir` for `session` at `now`. The output line,
/// or why not.
pub fn run_in(
    dir: &Path,
    session: &str,
    cmd: &Cmd,
    flags: &Flags,
    now: u64,
) -> Result<String, String> {
    run_in_code(dir, session, cmd, flags, now, None).map(|(msg, _)| msg)
}

/// [`run_in`], with the exit code (`claim` has several, [`resources::exit`])
/// and, for tests, the machine to claim against (else this one, detected).
pub fn run_in_code(
    dir: &Path,
    session: &str,
    cmd: &Cmd,
    flags: &Flags,
    now: u64,
    cap: Option<&resources::Capacity>,
) -> Result<(String, i32), String> {
    if session.is_empty() || session.contains(['/', '\\']) || session.starts_with('.') {
        return Err(format!("not a session id: {session:?}"));
    }
    // A name is the session's, not a manager's: no feed, no lease.
    if let Cmd::Name(given) = cmd {
        return name(dir, session, given.as_deref(), flags.claude_dir.as_deref()).map(|m| (m, 0));
    }
    // The watch waits on the clock: the command line's own (`main`).
    if *cmd == Cmd::Watch {
        return Err("`watch` runs only as its own command: `giverny manage watch`".into());
    }
    adopt_continued(dir, session, flags.claude_dir.as_deref());
    let ledger = resources::ledger_path(dir);
    // Every command from a session keeps its leases alive.
    if *cmd != Cmd::Path
        && let Err(e) = resources::heartbeat(&ledger, Some(dir), session, now, cap)
    {
        eprintln!("giverny manage: {e}");
    }
    // Its manager starting or running a task takes up a grant the queue
    // made for it on another session's call.
    if let Cmd::Start(task) | Cmd::Run(task) = cmd
        && let Err(e) = resources::collect_at(&ledger, session, task, now)
    {
        eprintln!("giverny manage: {e}");
    }
    match cmd {
        Cmd::Claim(task) => return claim(dir, &ledger, session, task, flags, now, cap),
        Cmd::Run(task) => {
            return crate::manage_run::run(dir, &ledger, session, task, flags, cap);
        }
        Cmd::Release(task) => {
            let gone = resources::release_at(&ledger, Some(dir), session, task, now, cap)?;
            resources::annotate_row(dir, session, task, None);
            return Ok((
                match gone {
                    Some(l) => format!("released {task}: {}", l.describe()),
                    None => format!(
                        "{task} held no lease{}",
                        held_elsewhere(&ledger, session, task, now)
                    ),
                },
                0,
            ));
        }
        Cmd::Ask(target, text) => {
            let msg = crate::manage_inbox::ask(
                dir,
                &ledger,
                session,
                target,
                text,
                flags.task.as_deref(),
                flags.priority.as_deref(),
                now,
            )?;
            return Ok((msg, 0));
        }
        Cmd::Reply(id, text) => {
            let msg = crate::manage_inbox::reply(dir, &ledger, session, id, text, now)?;
            return Ok((msg, 0));
        }
        Cmd::Accuracy => return Ok((accuracy(dir, flags.repo.as_deref()), 0)),
        Cmd::Resources => {
            let cap = match cap {
                Some(c) => c.clone(),
                None => resources::Capacity::detect(&ledger)?,
            };
            let eta = |s: &str, t: &str| resources::eta_left_s(dir, s, t, now);
            let peaks = Peaks::load(dir);
            let left = |s: &str, t: &str| resources::eta_or_estimate_s(dir, s, t, now);
            let out = resources::with_ledger(&ledger, now, |l| {
                peaks.fill(l);
                l.advance(&cap, now, &left, None);
                resources::report(l, &cap, now, &eta)
            })?;
            return Ok((out, 0));
        }
        _ => {}
    }
    // A task with anything of it still running is not done: `land` and
    // `drop` refuse it, whatever the outcome, with no override, and change
    // nothing. Giverny gates its own command; it stops
    // nothing.
    if let Cmd::Land(task) | Cmd::Drop(task) = cmd {
        let verb = if matches!(cmd, Cmd::Land(_)) {
            "land"
        } else {
            "drop"
        };
        let still = task_still_runs(dir, &ledger, session, task, flags, now);
        if let Some(why) = refusal(
            &format!("`{task}` still runs, so it is not done and does not {verb}"),
            &format!("`giverny-manage {verb} {task}`"),
            &still,
            now,
        ) {
            return Err(why);
        }
    }
    let (mut msg, landed) = match run_feed(dir, session, cmd, flags, now) {
        Ok(x) => x,
        // A task that is over gives its lease back even when its row could
        // not be changed (none in this manager session, another writer's feed).
        Err(e) => {
            if let Cmd::Land(task) | Cmd::Drop(task) = cmd
                && let Some(l) = resources::release_at(&ledger, Some(dir), session, task, now, cap)?
            {
                return Err(format!("{e}; released {}", l.describe()));
            }
            return Err(e);
        }
    };
    // A pause stops the clock only: what still runs for the task — its
    // worker mid-turn, its `run` commands, its worker's
    // background shells — is named for the manager to stop or
    // keep, and the task is not paused until all of it stops; or it says
    // nothing of the task runs. Giverny stops nothing.
    if let Cmd::Pause(task) = cmd {
        let still = task_still_runs(dir, &ledger, session, task, flags, now);
        let head = format!("paused {task}");
        let rest = msg.strip_prefix(&head).unwrap_or(&msg).to_string();
        msg = format!("{}{rest}", pause_report(task, &still, now));
    }
    // Resume names the background shells the task's worker left running,
    // with how to stop each. Giverny stops nothing.
    if let Cmd::Resume(task) = cmd {
        let live = crate::bg_shells::task_shells(dir, session, task, &crate::bg_shells::System);
        msg.push_str(&crate::bg_shells::still_running(task, &live, now));
    }
    // A task that lands — by `land`, or handed off by a `start --agent` — or
    // is dropped gives its lease back, after the feed's lock is let go (the
    // ledger's is never taken under it).
    let mut free = landed;
    if let Cmd::Land(task) | Cmd::Drop(task) = cmd
        && !free.contains(task)
    {
        free.push(task.clone());
    }
    for task in &free {
        if let Some(l) = resources::release_at(&ledger, Some(dir), session, task, now, cap)? {
            msg = if free.len() > 1 {
                format!("{msg}; released {task}: {}", l.describe())
            } else {
                format!("{msg}; released {}", l.describe())
            };
        }
    }
    Ok((msg, 0))
}

/// What `pause` adds for a task's `run` commands still going: each one's
/// command, how long it has run, its pid and the command that stops it
/// (`kill <pid>`: its `run` stops the command's scope, or its whole tree,
/// and records). Nothing when none is.
fn still_running(task: &str, runs: &[crate::run_live::LiveRun], now: u64) -> String {
    if runs.is_empty() {
        return String::new();
    }
    let n = runs.len();
    let (what, it) = if n == 1 {
        (
            "1 `giverny-manage run` command still runs".to_string(),
            "it",
        )
    } else {
        (
            format!("{n} `giverny-manage run` commands still run"),
            "them",
        )
    };
    let mut out = format!(
        "\n  {what} for {task}, and Giverny stops nothing. Have the worker finish or stop \
         {it} at a safe point, or stop {it} yourself:"
    );
    for line in run_lines(runs, now) {
        out.push_str(&format!("\n    {line}"));
    }
    out
}

/// What `pause` says: that nothing of `task` runs, so it is
/// paused; or that only its clock is, with its worker still mid-turn, its
/// `run` commands and its worker's shells each named, and that the task is
/// not paused until they stop. A manager that told the user "paused" on a
/// bare `paused g483` left workers running for minutes under that word.
pub fn pause_report(task: &str, s: &StillRuns, now: u64) -> String {
    if s.is_empty() {
        return format!("paused {task}: nothing of {task} runs");
    }
    let mut out = match &s.working {
        Some((agent, t)) => {
            let ago = match now.saturating_sub(*t) / 1000 {
                n @ 0..60 => format!("{n}s"),
                n => feed::fmt_span(n as i64),
            };
            format!(
                "paused {task}'s clock; its worker {agent} is still working (last wrote {ago} \
                 ago), so {task} is not paused until it ends its turn: tell the user it is \
                 paused only then. Giverny tells you when it stops, and stops nothing"
            )
        }
        None => {
            let mut out = format!(
                "paused {task}'s clock; what is named below still runs for it, so {task} is \
                 not paused until that stops: tell the user it is paused only then"
            );
            if let Some(w) = &s.worker {
                out.push_str(&format!("\n  {w}"));
            }
            out
        }
    };
    out.push_str(&still_running(task, &s.runs, now));
    out.push_str(&crate::bg_shells::still_running(task, &s.shells, now));
    out
}

/// How long a worker's transcript may go unwritten and the worker still
/// count as mid-turn for `land`'s refusal. A working worker
/// writes a line at least as each tool call returns, and a Bash call is
/// moved to the background after ten minutes at most; a worker silent for
/// longer has gone without a report (its claude restarted, say), and only
/// its runs and shells, which `/proc` shows, can still hold the task.
pub const WORKER_QUIET_MS: u64 = 15 * 60 * 1000;

/// What still runs for a task, as `land` and `drop` look before they act:
/// its worker mid-turn or asleep on its own shells, its
/// live `giverny-manage run` commands, its worker's live background
/// shells.
#[derive(Debug, Clone, Default)]
pub struct StillRuns {
    /// The line for its worker, when it is mid-turn or asleep.
    pub worker: Option<String>,
    /// Its worker and when it last wrote, when it is mid-turn
    /// (`pause` says the task is not paused until it ends its turn).
    pub working: Option<(String, u64)>,
    pub runs: Vec<crate::run_live::LiveRun>,
    pub shells: Vec<crate::bg_shells::Live>,
}

impl StillRuns {
    pub fn is_empty(&self) -> bool {
        self.worker.is_none() && self.runs.is_empty() && self.shells.is_empty()
    }
}

/// The line for a task's worker that keeps it from landing, or none: one
/// mid-turn (written within [`WORKER_QUIET_MS`]) is still working; one
/// that ended its turn with shells of its own still running is asleep on
/// them.
pub fn worker_line(
    agent: &str,
    state: WorkerState,
    asleep: &[crate::bg_shells::Shell],
    now: u64,
) -> Option<String> {
    let ago = |t: u64| match now.saturating_sub(t) / 1000 {
        s @ 0..60 => format!("{s}s"),
        s => feed::fmt_span(s as i64),
    };
    match state {
        WorkerState::Working { last_ms: Some(t) } if now.saturating_sub(t) < WORKER_QUIET_MS => {
            Some(format!(
                "its worker {agent} is still working (last wrote {} ago): wait for its report",
                ago(t)
            ))
        }
        WorkerState::Ended { at_ms } if !asleep.is_empty() => Some(format!(
            "its worker {agent} ended its turn {} ago and is asleep, {}: Claude Code wakes it \
             when {} ends. Stop the shells below, or wait for its report",
            ago(at_ms),
            crate::bg_shells::waiting_on(asleep, now),
            if asleep.len() == 1 { "that" } else { "one" },
        )),
        _ => None,
    }
}

/// Why a command refuses to land a task that still runs (`head`), with
/// each live thing and how to stop it, and what to run again once they
/// are stopped (`again`); `None` when nothing runs.
pub fn refusal(head: &str, again: &str, s: &StillRuns, now: u64) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let mut out = format!(
        "{head}: a task with anything still running is not done. Giverny stops nothing, and \
         nothing overrides this:"
    );
    if let Some(w) = &s.worker {
        out.push_str(&format!("\n  {w}"));
    }
    if !s.shells.is_empty() {
        let what = match s.shells.len() {
            1 => "1 background shell its worker started still runs".to_string(),
            n => format!("{n} background shells its worker started still run"),
        };
        out.push_str(&format!("\n  {what}:"));
        for line in crate::bg_shells::lines(&s.shells, now) {
            out.push_str(&format!("\n    {line}"));
        }
    }
    if !s.runs.is_empty() {
        let what = match s.runs.len() {
            1 => "1 `giverny-manage run` command still runs".to_string(),
            n => format!("{n} `giverny-manage run` commands still run"),
        };
        out.push_str(&format!("\n  {what}:"));
        for line in run_lines(&s.runs, now) {
            out.push_str(&format!("\n    {line}"));
        }
    }
    out.push_str(&format!(
        "\n  Nothing was changed. Once they are stopped or done, run {again} again."
    ));
    Some(out)
}

/// What still runs for `task` in `session` ([`StillRuns`]): its live runs
/// and its worker's live shells always, and with `worker` its worker's
/// turn, read from the manager session's file and the transcripts under
/// the Claude config dir given (else this machine's).
/// The worker counts only while the task is its current one: a Running row
/// none of its later Running rows came after (a batch it was spawned with
/// is all current).
fn still_runs(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    worker: Option<(&Value, Option<&Path>)>,
    now: u64,
) -> StillRuns {
    let runs = crate::run_live::task_runs(&crate::run_live::runs_dir(ledger), session, task);
    let shells = crate::bg_shells::task_shells(dir, session, task, &crate::bg_shells::System);
    let worker = worker.and_then(|(doc, cfg)| {
        let agent = current_worker(doc, task)?;
        {
            let sessions = continuation::ids(doc);
            let state = worker_state(cfg, &sessions, &agent);
            // Killed with its session: nothing of it runs,
            // however recently it wrote.
            if died_with_session(doc, task, state).is_some() {
                return None;
            }
            let asleep: Vec<crate::bg_shells::Shell> = shells
                .iter()
                .map(|l| l.shell.clone())
                .filter(|s| s.agent == agent)
                .collect();
            let mid_turn = match state {
                WorkerState::Working { last_ms: Some(t) }
                    if now.saturating_sub(t) < WORKER_QUIET_MS =>
                {
                    Some((agent.clone(), t))
                }
                _ => None,
            };
            worker_line(&agent, state, &asleep, now).map(|l| (l, mid_turn))
        }
    });
    let (worker, working) = match worker {
        Some((l, w)) => (Some(l), w),
        None => (None, None),
    };
    StillRuns {
        worker,
        working,
        runs,
        shells,
    }
}

/// When row `task`'s worker died with its session: the row
/// went off then (`went_off`, kept past its revive) and the worker, in
/// `state`, has not written since. `None` for a worker that lives.
fn died_with_session(doc: &Value, task: &str, state: WorkerState) -> Option<u64> {
    let rows = doc.get("rows").and_then(Value::as_array)?;
    let row = rows.get(find(rows, task)?)?.as_object()?;
    let off = ms_of(row, "went_off")?;
    match state {
        WorkerState::Working { last_ms: Some(t) } | WorkerState::Ended { at_ms: t } if t > off => {
            None
        }
        _ => Some(off),
    }
}

/// The worker of the Running row `task` when that task is its current one:
/// no Running row of the same worker started [`feed::LATER_TASK_MS`] or
/// more after it.
fn current_worker(doc: &Value, task: &str) -> Option<String> {
    let rows = doc.get("rows").and_then(Value::as_array)?;
    let row = rows.get(find(rows, task)?)?.as_object()?;
    if stage_of(row) != Some(feed::Stage::Running) {
        return None;
    }
    let agent = row.get("agent_id").and_then(Value::as_str)?;
    let started = ms_of(row, "started").unwrap_or(0);
    let later = rows.iter().filter_map(Value::as_object).any(|r| {
        r.get("key").and_then(Value::as_str) != Some(task)
            && r.get("agent_id").and_then(Value::as_str) == Some(agent)
            && stage_of(r) == Some(feed::Stage::Running)
            && ms_of(r, "started").is_some_and(|s| s >= started.saturating_add(feed::LATER_TASK_MS))
    });
    (!later).then(|| agent.to_string())
}

/// [`still_runs`] for `land` and `drop`: the session's file read as it is
/// (no lock: nothing is written), the worker's turn counted.
fn task_still_runs(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    now: u64,
) -> StillRuns {
    let doc = std::fs::read(file_for(dir, session))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|d| writer_of(d).is_none_or(|w| w == WRITER));
    still_runs(
        dir,
        ledger,
        session,
        task,
        doc.as_ref().map(|d| (d, flags.claude_dir.as_deref())),
        now,
    )
}

/// Each live run as a line: `` `sleep 300`, 8m in, pid 4242: `kill 4242`
/// stops it and all it started ``.
pub(crate) fn run_lines(runs: &[crate::run_live::LiveRun], now: u64) -> Vec<String> {
    runs.iter()
        .map(|r| {
            let what = r
                .cmd
                .as_deref()
                .map_or("(command not recorded)".into(), short_cmd);
            let ran = r.started_ms.map_or(String::new(), |t| {
                let s = now.saturating_sub(t) / 1000;
                match s {
                    0..60 => format!(", {s}s in"),
                    _ => format!(", {} in", feed::fmt_span(s as i64)),
                }
            });
            format!(
                "{what}{ran}, pid {pid}: `kill {pid}` stops it and all it started",
                pid = r.pid
            )
        })
        .collect()
}

/// A command as a one-line listing quotes it: in backticks, held to about
/// 60 characters.
pub(crate) fn short_cmd(cmd: &str) -> String {
    let one: String = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 60;
    let cut = match one.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &one[..i]),
        None => one,
    };
    format!("`{}`", cut.replace('`', "'"))
}

/// `release` of a task this session holds no lease for, which another
/// session does: whose it is and how to ask for it. A release
/// only ever gives back the caller's own.
fn held_elsewhere(ledger: &Path, session: &str, task: &str, now: u64) -> String {
    let Some(l) = crate::manage_inbox::read_ledger(ledger, now) else {
        return String::new();
    };
    match l
        .leases
        .iter()
        .find(|x| x.task == task && x.session != session)
    {
        Some(x) => format!(
            " of this session's; another session ({}) holds {task}'s, and only it can \
             give it back: `giverny-manage ask {task} \"<what you need and why>\"`",
            x.session.get(..8).unwrap_or(&x.session)
        ),
        None => String::new(),
    }
}

/// `claim`: ask the ledger, and copy the answer onto the task's feed row.
fn claim(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    now: u64,
    cap: Option<&resources::Capacity>,
) -> Result<(String, i32), String> {
    let cap = match cap {
        Some(c) => c.clone(),
        None => resources::Capacity::detect(ledger)?,
    };
    let repo = flags.repo.clone().or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_default();
        manage_history::repo_of(task, &cwd)
    });
    let peaks = Peaks::load(dir);
    let req = resources::Request {
        peak_mb: peaks.expected(session, task, repo.as_deref()),
        ..flags.request()
    };
    // Figures given on a held lease, each no larger, shrink it in place: the
    // answer to an ask. Otherwise it is a claim as ever.
    let (out, shrunk) = resources::with_ledger(ledger, now, |l| {
        peaks.fill(l);
        if let Some(s) = l.shrink(session, task, flags.ram_mb, flags.vram_mb, now) {
            l.heartbeat(session, now);
            return (resources::Outcome::Held(s.1.clone()), Some(s.0));
        }
        let left = |s: &str, t: &str| resources::eta_or_estimate_s(dir, s, t, now);
        (
            l.claim_timed(&cap, session, task, repo.as_deref(), &req, now, &left),
            None,
        )
    })?;
    resources::annotate_row(dir, session, task, resources::row_lease(&out, &req));
    let eta = |s: &str, t: &str| resources::eta_left_s(dir, s, t, now);
    // The note on an old `--cpu` goes first, on a line of its own.
    let note = flags
        .cpu_note()
        .map(|n| format!("{n}\n"))
        .unwrap_or_default();
    if let (Some(was), resources::Outcome::Held(l)) = (&shrunk, &out) {
        return Ok((
            format!(
                "{note}shrunk {task}: {} (was {})",
                l.describe(),
                was.describe()
            ),
            resources::exit::GRANTED,
        ));
    }
    let mut line = format!("{note}{}", resources::outcome_line(task, &out, &eta));
    if let resources::Outcome::Held(l) = &out {
        let grow = flags.ram_mb.is_some_and(|r| r > l.ram_mb)
            || flags
                .vram_mb
                .is_some_and(|v| v > l.vram_mb && !l.gpus.is_empty());
        if grow {
            line.push_str("; a held lease is not grown: release it and claim again");
        }
    }
    if let resources::Outcome::Queued { blockers, .. } = &out
        && let Some(hint) = resources::ask_hint(
            task,
            blockers,
            resources::eta_or_estimate_s(dir, session, task, now),
            &eta,
        )
    {
        line.push_str(&format!("; {hint}"));
    }
    if !matches!(out, resources::Outcome::Refused(_))
        && let Some(hint) = peaks.hint(session, task, repo.as_deref())
    {
        line.push_str(&format!(" ({hint})"));
    }
    Ok((line, out.exit_code()))
}

/// How often `claim --wait` re-claims: the queue also moves on other
/// sessions' calls, and a grant waits 10 minutes to be taken up.
pub const WAIT_POLL_MS: u64 = 30_000;

/// `claim <task> --wait`: claim, and while the answer is
/// `queued`, re-claim every [`WAIT_POLL_MS`] until it is anything else
/// (granted, granted smaller, refused, an error). The first queued answer
/// goes to `say` at once (it may suggest an `ask`); the final one, with how
/// long it waited, is returned with its exit code. `clock` and `nap` are the
/// time's, so tests run it without waiting.
#[allow(clippy::too_many_arguments)]
pub fn claim_wait(
    dir: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    cap: Option<&resources::Capacity>,
    clock: &mut dyn FnMut() -> u64,
    nap: &mut dyn FnMut(u64),
    say: &mut dyn FnMut(&str),
) -> Result<(String, i32), String> {
    let cmd = Cmd::Claim(task.to_string());
    let from = clock();
    let mut told = false;
    loop {
        let now = clock();
        let (msg, code) = run_in_code(dir, session, &cmd, flags, now, cap)?;
        if code != resources::exit::QUEUED {
            let waited = now.saturating_sub(from);
            return Ok(if told {
                (format!("{msg} (after {} queued)", fmt_wait(waited)), code)
            } else {
                (msg, code)
            });
        }
        if !told {
            say(&format!(
                "{msg}\nwaiting: re-claiming every 30s until granted or refused"
            ));
            told = true;
        }
        nap(WAIT_POLL_MS);
    }
}

/// A wait as `claim --wait` says it: `40s`, `12m`, `1h05m`.
fn fmt_wait(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// What past tasks peaked at under `giverny manage run`: the history,
/// read once, and the feeds that give a task its kind.
pub(crate) struct Peaks<'a> {
    dir: &'a Path,
    history: Vec<manage_history::Record>,
}

impl<'a> Peaks<'a> {
    pub(crate) fn load(dir: &'a Path) -> Peaks<'a> {
        let history = manage_history::path(dir)
            .map(|p| manage_history::load(&p))
            .unwrap_or_default();
        Peaks { dir, history }
    }

    /// The type word of `session`'s row for `task` (`BUG`, …).
    fn kind(&self, session: &str, task: &str) -> Option<String> {
        let title = feed::find(self.dir, session).and_then(|(_, f)| {
            f.rows
                .into_iter()
                .find(|r| r.key == task)
                .and_then(|r| r.title)
        })?;
        manage_history::kind_of(&title)
    }

    /// For `claim` to say beside its answer: `the last 4 BUG tasks in demo
    /// peaked at 1.8G (median), 2.6G at most`.
    pub(crate) fn hint(&self, session: &str, task: &str, repo: Option<&str>) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        let kind = self.kind(session, task);
        manage_history::peak_hint(&self.history, repo, kind.as_deref())
    }

    /// What admission counts the task at: the high end of
    /// what tasks like it peaked at.
    pub(crate) fn expected(&self, session: &str, task: &str, repo: Option<&str>) -> Option<u64> {
        if self.history.is_empty() {
            return None;
        }
        let kind = self.kind(session, task);
        manage_history::expected_peak_mb(&self.history, repo, kind.as_deref())
    }

    /// [`Peaks::expected`] for a lease already in the ledger.
    pub(crate) fn of_lease(&self, l: &resources::Lease) -> Option<u64> {
        self.expected(&l.session, &l.task, l.repo.as_deref())
    }

    /// Give the ledger's leases that have none their expected peak.
    pub(crate) fn fill(&self, l: &mut resources::Ledger) {
        l.fill_peaks(&|x| self.of_lease(x));
    }
}

/// Change `session`'s feed row for `task` under the feed's lock, when the
/// feed is this writer's and has the row. False when nothing was changed.
pub(crate) fn edit_row(
    dir: &Path,
    session: &str,
    task: &str,
    f: impl FnOnce(&mut Map<String, Value>),
) -> bool {
    let file = file_for(dir, session);
    if !file.exists() {
        return false;
    }
    let Ok(_lock) = Lock::take(&file) else {
        return false;
    };
    let Some(mut doc) = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return false;
    };
    if writer_of(&doc).is_some_and(|w| w != WRITER) {
        return false;
    }
    let Some(row) = doc
        .get_mut("rows")
        .and_then(Value::as_array_mut)
        .and_then(|rows| {
            rows.iter_mut()
                .find(|r| r.get("key").and_then(Value::as_str) == Some(task))
        })
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    f(row);
    write(&file, &doc).is_ok()
}

/// Open (`Some(note)`) or close (`None`) a waiting span on a Running row,
/// as `eta --why wait` and the next plain `eta` do: `run` waiting on a slot
/// or a queued lease is not work. Opens only where no wait or pause is
/// open; true when it opened one (only that one is closed later).
pub(crate) fn mark_waiting(
    dir: &Path,
    session: &str,
    task: &str,
    note: Option<&str>,
    now: u64,
) -> bool {
    let mut opened = false;
    edit_row(dir, session, task, |row| match note {
        Some(n) => {
            if stage_of(row) == Some(feed::Stage::Running)
                && !row.contains_key("waiting_since")
                && !row.contains_key("paused_since")
            {
                row.insert("waiting_since".into(), json!(stamp(now)));
                row.insert("note".into(), json!(n));
                opened = true;
            }
        }
        None => close_wait(row, now),
    });
    opened
}

/// The feed commands: read the session's feed, change it, write it back.
/// The message, and the keys the command landed (whose leases go back).
fn run_feed(
    dir: &Path,
    session: &str,
    cmd: &Cmd,
    flags: &Flags,
    now: u64,
) -> Result<(String, Vec<String>), String> {
    let cfg = flags.claude_dir.as_deref();
    let file = file_for(dir, session);
    if *cmd == Cmd::Path {
        return Ok((file.display().to_string(), Vec::new()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _lock = Lock::take(&file)?;
    // What the machine slept through comes off every clock here.
    let sleeps = sleeps::load(dir);
    let existing = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    if *cmd == Cmd::ClearDone {
        return Ok((clear_done(&file, existing), Vec::new()));
    }
    if let Some(doc) = &existing
        && let Some(w) = writer_of(doc)
        && w != WRITER
    {
        return Err(format!(
            "{} is written by {w}; giverny manage leaves it alone",
            file.display()
        ));
    }
    match cmd {
        Cmd::Show => {
            let mut doc = existing.unwrap_or_else(|| new_doc(session));
            // The session's name lives beside the feed.
            if let (Some(n), Some(obj)) = (names::read(dir, session), doc.as_object_mut()) {
                obj.insert("name".into(), json!(n));
            }
            Ok((show(&doc, now, &sleeps), Vec::new()))
        }
        Cmd::Clear => {
            if existing.is_some() {
                std::fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            }
            Ok(("cleared".into(), Vec::new()))
        }
        _ => {
            let mut doc = existing.unwrap_or_else(|| new_doc(session));
            let obj = doc.as_object_mut().expect("filtered to objects");
            obj.insert("writer".into(), json!(WRITER));
            obj.entry("version").or_insert(json!(feed::FEED_VERSION));
            obj.entry("session").or_insert(json!(session));
            if doc.get("root").is_none() {
                continuation::stamp_root(&mut doc, continuation::root_of(cfg, session).as_deref());
            }
            let sessions = continuation::ids(&doc);
            let history = manage_history::path(dir);
            let mut flags = flags.clone();
            // A worker spawned for the task (its description names it) is
            // not being reused, however much it has read.
            let spawned_for = |task: &str, agent: &str| {
                agent_description(cfg, &sessions, agent).is_some_and(|d| feed::names_key(&d, task))
            };
            let heavy_note = match (cmd, &flags.agent) {
                (Cmd::Start(task) | Cmd::Eta(task, _), Some(agent))
                    if row_agent(&doc, task) != Some(agent.as_str())
                        && !spawned_for(task, agent) =>
                {
                    heavy_guard(
                        task,
                        agent,
                        worker_context(cfg, &sessions, agent),
                        flags.heavy_ok,
                    )?
                }
                _ => None,
            };
            let said = tell_record(&doc, cmd, &mut flags, history.as_deref());
            if let (Cmd::Start(_) | Cmd::Eta(..), Some(agent), None) =
                (cmd, &flags.agent, &flags.agent_desc)
                && needs_description(&doc, now)
            {
                flags.agent_desc = agent_description(cfg, &sessions, agent);
            }
            let before = done_keys(&doc);
            let read_log = |agent: &str| worker_log(cfg, &sessions, agent);
            let resumed = if has_unworkered(&doc) || has_worked(&doc) {
                let spawned = spawns(cfg, &sessions);
                stamp_spawns(&mut doc, &spawned);
                let notices = std::cell::OnceCell::new();
                let finished = |agent: &str, last: Option<u64>| {
                    let notices = notices.get_or_init(|| {
                        let cfg = cfg.map(Path::to_path_buf).or_else(continuation::config_dir);
                        sessions
                            .iter()
                            .filter_map(|s| {
                                crate::subagents::session_transcript(cfg.as_deref()?, s)
                            })
                            .map(|t| crate::subagents::read_completions(&t))
                            .collect::<Vec<_>>()
                    });
                    notices
                        .iter()
                        .find_map(|n| n.get(agent))
                        .is_some_and(|c| crate::subagents::is_finished(c, last))
                };
                relink_resumed(
                    &mut doc,
                    &spawned,
                    |a| last_written(cfg, &sessions, a),
                    finished,
                )
            } else {
                Vec::new()
            };
            // A hand-off lands the worker's earlier task: refused, as
            // `land` is, while anything of it still runs.
            // Its worker is the one being handed on, so only its runs and
            // shells count.
            let handing = match (cmd, flags.agent.as_deref()) {
                (Cmd::Start(task), Some(agent)) => Some((task, agent)),
                (Cmd::Eta(task, _), Some(agent)) if row_agent(&doc, task) != Some(agent) => {
                    Some((task, agent))
                }
                _ => None,
            };
            if let Some((task, agent)) = handing {
                let ledger = resources::ledger_path(dir);
                for earlier in handed_keys(&doc, task, agent, flags.agent_desc.as_deref(), now) {
                    let still = still_runs(dir, &ledger, session, &earlier, None, now);
                    if let Some(why) = refusal(
                        &format!(
                            "handing {agent} on to `{task}` lands `{earlier}`, which still runs"
                        ),
                        &match cmd {
                            Cmd::Start(_) => {
                                format!("`giverny-manage start {task} --agent {agent}`")
                            }
                            _ => format!("your `giverny-manage eta {task} … --agent {agent}`"),
                        },
                        &still,
                        now,
                    ) {
                        return Err(why);
                    }
                }
            }
            let msg = apply(&mut doc, cmd, &flags, now, &sleeps)?;
            let handed = match cmd {
                Cmd::Start(_) | Cmd::Eta(..) => flags.agent.as_deref(),
                _ => None,
            };
            freeze_tokens(&mut doc, &before, handed, read_log);
            let mut msg = msg;
            // A resume restarts the clock only: whether anyone works on the
            // task now, and how to wake it, is the manager's to see.
            // Giverny wakes nothing.
            if let Cmd::Resume(key) = cmd {
                let agent = row_agent(&doc, key).map(str::to_string);
                let state = agent.as_deref().map(|a| worker_state(cfg, &sessions, a));
                // Ended with a shell of its own still running: asleep, and
                // Claude Code wakes it.
                let asleep: Vec<crate::bg_shells::Shell> = match (&agent, state) {
                    (Some(a), Some(WorkerState::Ended { .. })) => {
                        crate::bg_shells::task_shells(dir, session, key, &crate::bg_shells::System)
                            .into_iter()
                            .map(|l| l.shell)
                            .filter(|s| &s.agent == a)
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let killed = state.and_then(|st| died_with_session(&doc, key, st));
                msg.push_str(&resume_note(
                    key,
                    agent.as_deref(),
                    state,
                    row_wait(&doc, key),
                    &asleep,
                    killed,
                    now,
                ));
            }
            if let (Cmd::Start(task), None) = (cmd, &flags.agent)
                && row_agent(&doc, task).is_none()
            {
                let idle: Vec<Idle> = idle_workers(&doc, task, now)
                    .into_iter()
                    .map(|(a, k)| {
                        let c = worker_context(cfg, &sessions, &a);
                        (a, k, c)
                    })
                    .collect();
                if let Some(hint) = idle_hint(task, &idle) {
                    msg = format!("{msg}\n{hint}");
                }
            }
            if let Some(note) = heavy_note {
                msg = format!("{msg}\n{note}");
            }
            write(&file, &doc).map_err(|e| format!("{}: {e}", file.display()))?;
            if let Cmd::Eta(..) = cmd {
                crate::manage_progress::mark(dir, session, &doc);
            }
            if let Some(h) = &history {
                learn(&doc, &before, session, h, &sleeps);
            }
            let mut msg = match said {
                Some(said) => format!("{msg}{said}"),
                None => msg,
            };
            if !resumed.is_empty() {
                msg.push_str(&format!(
                    "\n  {} moved to the worker resuming it: its first worker had finished",
                    resumed.join(", ")
                ));
            }
            let landed = done_keys(&doc)
                .into_iter()
                .filter(|k| !before.contains(k))
                .collect();
            let msg = match cmd {
                Cmd::Plan(key) if !has_brief(&doc, key) => format!("{msg}\n{}", no_brief(key)),
                _ => msg,
            };
            Ok((msg, landed))
        }
    }
}

/// What a task's worker is doing, as `resume` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Its transcript was last written then, after its latest finish.
    Working { last_ms: Option<u64> },
    /// It ended its turn then: its finish notice is as new as its last turn.
    Ended { at_ms: u64 },
}

/// Worker `agent`'s state: ended when the session's transcript holds a
/// finish notice for it as new as its last turn ([`crate::subagents::is_finished`]).
fn worker_state(cfg: Option<&Path>, sessions: &[String], agent: &str) -> WorkerState {
    let last = last_written(cfg, sessions, agent);
    let cfg = cfg.map(Path::to_path_buf).or_else(continuation::config_dir);
    let done = sessions
        .iter()
        .filter_map(|s| crate::subagents::session_transcript(cfg.as_deref()?, s))
        .find_map(|t| crate::subagents::read_completions(&t).remove(agent));
    match done.filter(|c| crate::subagents::is_finished(c, last)) {
        Some(c) => WorkerState::Ended { at_ms: c.at_ms },
        None => WorkerState::Working { last_ms: last },
    }
}

/// The row `key`'s open wait: since when, and its note.
fn row_wait(doc: &Value, key: &str) -> Option<(u64, Option<String>)> {
    let row = doc
        .get("rows")
        .and_then(Value::as_array)
        .and_then(|rows| find(rows, key).map(|i| &rows[i]))
        .and_then(Value::as_object)?;
    let since = ms_of(row, "waiting_since")?;
    let note = row.get("note").and_then(Value::as_str).map(str::to_string);
    Some((since, note))
}

/// What `resume` adds under its line: whether the task's
/// worker is working or has ended its turn and needs waking, and how; or
/// that none holds it; and a wait it marked that is still open. A worker
/// that ended its turn with background shells of its own still running
/// (`asleep`) is asleep: Claude Code wakes it when one ends.
/// One `killed` with its session at that instant is dead.
pub fn resume_note(
    key: &str,
    agent: Option<&str>,
    state: Option<WorkerState>,
    wait: Option<(u64, Option<String>)>,
    asleep: &[crate::bg_shells::Shell],
    killed: Option<u64>,
    now: u64,
) -> String {
    let ago = |t: u64| feed::fmt_span((now.saturating_sub(t) / 1000) as i64);
    let mut out = match (agent, state) {
        // Killed with its session, whatever it last wrote.
        (Some(a), _) if killed.is_some() => format!(
            "\n  its worker {a} was killed with this session {} ago and works no more: a \
             fresh worker whose description names {key} takes the row in the pane, and \
             `giverny-manage start {key} --agent <its id>` records it; else spawn one with a \
             brief, or hand {key} to an idle one the same way. Giverny wakes nothing.",
            ago(killed.unwrap_or(now))
        ),
        (Some(a), Some(WorkerState::Ended { at_ms })) if !asleep.is_empty() => format!(
            "\n  its worker {a} ended its turn {} ago and is asleep, {}: Claude Code wakes \
             it when {} ends. Nothing to wake; if it should not wait, stop the shell (below) \
             or message it with SendMessage (to: \"{a}\"). Giverny wakes nothing.",
            ago(at_ms),
            crate::bg_shells::waiting_on(asleep, now),
            if asleep.len() == 1 { "that" } else { "one" },
        ),
        (Some(a), Some(WorkerState::Ended { at_ms })) => format!(
            "\n  its worker {a} ended its turn {} ago and will not carry on by itself: wake \
             it with SendMessage (to: \"{a}\") saying what to do next, or spawn a fresh \
             worker with a brief and `giverny-manage start {key} --agent <its id>`. Giverny \
             wakes nothing.",
            ago(at_ms)
        ),
        (Some(a), Some(WorkerState::Working { last_ms: Some(t) })) => {
            format!("\n  its worker {a} is working (last wrote {} ago)", ago(t))
        }
        (Some(a), _) => format!(
            "\n  its worker {a} has written nothing Giverny can read: check it is working, \
             or wake it with SendMessage (to: \"{a}\")"
        ),
        (None, _) => format!(
            "\n  no worker holds {key}: spawn one with a brief, or hand it to an idle one \
             (`giverny-manage start {key} --agent <id>`)"
        ),
    };
    if let Some((since, note)) = wait {
        let why = note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map_or(String::new(), |n| format!(" ({n})"));
        out.push_str(&format!(
            "\n  its wait{why} is still open, {} in: off its working time until the worker's \
             next `eta` without --why",
            ago(since)
        ));
    }
    out
}

/// The worker the row `key` has, if it has one.
fn row_agent<'a>(doc: &'a Value, key: &str) -> Option<&'a str> {
    doc.get("rows")
        .and_then(Value::as_array)
        .and_then(|rows| find(rows, key).map(|i| &rows[i]))
        .and_then(|r| r.get("agent_id"))
        .and_then(Value::as_str)
}

/// Whether the row `key` names a brief.
fn has_brief(doc: &Value, key: &str) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .and_then(|rows| find(rows, key).map(|i| &rows[i]))
        .and_then(|r| r.get("brief"))
        .and_then(Value::as_str)
        .is_some_and(|b| !b.trim().is_empty())
}

/// The line `plan` adds when its row has no brief.
fn no_brief(key: &str) -> String {
    format!(
        "  no brief: its Next up row opens to its title and note only; write the \
         worker's prompt (or the task's text) to a file and pass `--brief FILE` \
         (`giverny manage plan {key} --eta … --brief FILE`, or on `start`)"
    )
}

/// `plan`/`start` with `--eta N`, and a worker's first `eta` on a Running
/// row (its re-estimate): how the giver's instinct on such estimates runs,
/// from the history, told and never applied — the figure given is the figure
/// stored and shown. Also fills in the repo the row will remember, and for a
/// guess the factor its giver had been shown ([`Flags::told_x`]) and the
/// level of the line printed now ([`Flags::guess_level`]). Returns what to
/// add to the command's line, or `None` for a command that prints none.
///
/// **What the dispatcher had been shown**. The line `plan`
/// and `start` print comes after the figure, so a figure was given under
/// the line only when this manager session printed the same line before —
/// on an earlier `plan`/`start --eta` of a row whose guess was told at the
/// same level (same repo and kind, same repo, or all). Then the factor
/// counted as shown is the one in effect now, which is what that line
/// would say today; otherwise ×1. A worker's factor is the hook's stamp
/// on its row ([`apply`] reads it).
fn tell_record(
    doc: &Value,
    cmd: &Cmd,
    flags: &mut Flags,
    history: Option<&Path>,
) -> Option<String> {
    let rows = doc.get("rows").and_then(Value::as_array);
    let (key, track, given) = match cmd {
        Cmd::Plan(key) | Cmd::Start(key) => (key, manage_history::Track::Guess, flags.eta_s?),
        Cmd::Eta(key, left) => (key, manage_history::Track::Reestimate, *left),
        _ => return None,
    };
    let row = rows
        .and_then(|rows| find(rows, key).map(|i| &rows[i]))
        .and_then(Value::as_object);
    // A dispatcher's `eta` on a row not started yet is a new guess: its
    // factor is kept, its line not printed.
    let planned_eta = track == manage_history::Track::Reestimate
        && row.is_some_and(|r| stage_of(r) == Some(feed::Stage::Planned));
    let running = row.is_some_and(|r| stage_of(r) == Some(feed::Stage::Running));
    if track == manage_history::Track::Reestimate && !planned_eta && !running {
        // No row yet (`eta` starts one) or a landed one: nothing to tell.
        return None;
    }
    // A worker's first re-estimate is the scored one; a later one is not.
    let first = running && !row.is_some_and(|r| r.contains_key("reest_s"));
    let row_str = |k: &str| row.and_then(|r| r.get(k)).and_then(Value::as_str);
    if flags.repo.is_none() {
        flags.repo = row_str("repo").map(String::from).or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_default();
            manage_history::repo_of(key, &cwd)
        });
    }
    let title = flags.title.as_deref().or(row_str("title")).unwrap_or("");
    let kind = manage_history::kind_of(title);
    let past = history.map(manage_history::load).unwrap_or_default();
    let (repo, kind) = (flags.repo.as_deref(), kind.as_deref());
    // The gut figure, when one was given beside the figure.
    let gut = flags
        .gut_s
        .map(|g| format!(" (gut {})", feed::fmt_span(g as i64)))
        .unwrap_or_default();
    if track == manage_history::Track::Reestimate && !planned_eta {
        // How far off the gut runs reaches a worker only when it is asked
        // for an estimate. Its `eta` reply is such an ask when
        // the worker has not been told the line on this task yet, or gave
        // no gut figure: a figure is never taken without the
        // worker having seen the line.
        let told = row.is_some_and(|r| r.contains_key("track_x"));
        let ask = (flags.gut_s.is_none() || !told)
            .then(|| manage_history::instinct(&past, manage_history::Track::Reestimate, repo, kind))
            .flatten();
        let Some(ask) = ask else {
            return (first && !gut.is_empty()).then_some(gut);
        };
        flags.reest_x = Some(ask.x);
        let gut_min = (flags.gut_s.unwrap_or(given) + 30) / 60;
        let as_gut = if gut.is_empty() {
            ", taken as your gut figure".to_string()
        } else {
            gut
        };
        return Some(format!(
            "{as_gut}\n  {}. Give the figure you settle on for the minutes left: \
             `giverny-manage eta {key} <settled> --gut {gut_min} --note \"<why>\"`",
            crate::manage_nudge::capitalised(&ask.line()),
        ));
    }
    let told = manage_history::instinct(&past, manage_history::Track::Guess, repo, kind);
    let seen = |level: &str| {
        rows.into_iter()
            .flatten()
            .any(|r| r.get("guess_level").and_then(Value::as_str) == Some(level))
    };
    flags.told_x = Some(
        told.as_ref()
            .filter(|t| seen(&t.level))
            .map_or(1.0, |t| t.x),
    );
    if planned_eta {
        return None;
    }
    flags.guess_level = told.as_ref().map(|t| t.level.clone());
    let line = told
        .map(|t| {
            format!(
                "{}; {}: `--eta <settled> --gut <gut>`",
                t.line(),
                manage_history::GIVE_BOTH
            )
        })
        .unwrap_or_else(|| {
            let n = past.len();
            format!(
                "too few landed tasks to tell how far off your gut on such guesses runs \
                 ({n} in the history)"
            )
        });
    Some(format!(
        ": ~{}{gut}\n  {line}",
        feed::fmt_span(given as i64)
    ))
}

/// `accuracy`: the history's report.
fn accuracy(dir: &Path, repo: Option<&str>) -> String {
    match manage_history::path(dir) {
        Some(h) => manage_history::accuracy(&manage_history::load(&h), repo),
        None => format!("the history is off (${} is empty)", manage_history::ENV),
    }
}

/// The keys of the Done rows in a feed document.
fn done_keys(doc: &Value) -> Vec<String> {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|r| stage_of(r) == Some(feed::Stage::Done))
        .filter_map(|r| r.get("key").and_then(Value::as_str).map(String::from))
        .collect()
}

/// Append every row that landed in this command (Done now, not before) to
/// the history. A row that never started has no span and teaches nothing.
fn learn(doc: &Value, before: &[String], session: &str, history: &Path, sleeps: &[sleeps::Span]) {
    let rows = doc.get("rows").and_then(Value::as_array);
    for r in rows.into_iter().flatten().filter_map(Value::as_object) {
        let Some(key) = r.get("key").and_then(Value::as_str) else {
            continue;
        };
        if stage_of(r) != Some(feed::Stage::Done) || before.iter().any(|k| k == key) {
            continue;
        }
        if let Some(rec) = record_of(r, session, sleeps)
            && let Err(e) = manage_history::append(history, &rec)
        {
            eprintln!("giverny manage: {}: {e}", history.display());
        }
    }
}

/// A landed row as a history record: wall time from its true start
/// (`spawned` when a pause moved `started` on), working time that less its
/// paused and waiting spans and what the machine slept through.
pub fn record_of(
    r: &Map<String, Value>,
    session: &str,
    sleeps: &[sleeps::Span],
) -> Option<manage_history::Record> {
    let started = ms_of(r, "spawned").or_else(|| ms_of(r, "started"))?;
    let ended = ms_of(r, "ended")?;
    let wall_s = ended.saturating_sub(started) / 1000;
    let slept_s = sleeps::slept_ms(sleeps, started, ended) / 1000;
    let paused_s = u64_of(r, "paused_s").unwrap_or(0);
    let wait_s = u64_of(r, "wait_s").unwrap_or(0);
    let s = |k: &str| r.get(k).and_then(Value::as_str).map(String::from);
    let title = s("title");
    let usage_u64 = |k: &str| {
        r.get("usage")
            .and_then(Value::as_object)
            .and_then(|u| u64_of(u, k))
    };
    Some(manage_history::Record {
        key: s("key").unwrap_or_default(),
        kind: title.as_deref().and_then(manage_history::kind_of),
        title,
        repo: s("repo"),
        session: Some(session.to_string()),
        // `eta_guess_s`: the raw guess a row from an older version kept
        // beside its corrected `eta_s`.
        estimate_s: u64_of(r, "eta_guess_s")
            .or_else(|| u64_of(r, "eta_first_s"))
            .or_else(|| u64_of(r, "eta_s")),
        eta_s: u64_of(r, "eta_first_s").or_else(|| u64_of(r, "eta_s")),
        eta_final_s: u64_of(r, "eta_s"),
        // The factors shown with the guess and the re-estimate;
        // a row from before has none, which counts as ×1.
        gut_s: u64_of(r, "eta_gut_s"),
        told_x: r.get("eta_told_x").and_then(Value::as_f64),
        reest_s: u64_of(r, "reest_s"),
        reest_told_x: r.get("reest_told_x").and_then(Value::as_f64),
        reest_gut_s: u64_of(r, "reest_gut_s"),
        reest_at_s: u64_of(r, "reest_at_s"),
        wall_s,
        paused_s,
        wait_s,
        slept_s,
        work_s: wall_s.saturating_sub(paused_s + wait_s + slept_s),
        started: Some(stamp(started)),
        ended: Some(stamp(ended)),
        outcome: s("landing"),
        peak_mb: usage_u64("peak_mb"),
        cpu_s: usage_u64("cpu_s"),
        oom_kills: usage_u64("oom_kills").filter(|n| *n > 0),
    })
}

/// `clear-done` on the file: drop its Done rows when this writer owns it.
/// Another writer's feed is left as it is — the pane hides its Done rows
/// itself once asked ([`main`] asks it) — and so is a missing one.
fn clear_done(file: &Path, existing: Option<Value>) -> String {
    let Some(mut doc) = existing else {
        return "no feed for this session".into();
    };
    if let Some(w) = writer_of(&doc).filter(|w| *w != WRITER) {
        return format!("the feed is {w}'s: left as it is");
    }
    let Ok(rows) = rows_mut(&mut doc) else {
        return "the feed is not a JSON object: left as it is".into();
    };
    let before = rows.len();
    rows.retain(|r| r.as_object().and_then(stage_of) != Some(feed::Stage::Done));
    let gone = before - rows.len();
    if gone == 0 {
        return "no Done rows in the feed".into();
    }
    match write(file, &doc) {
        Ok(()) => format!(
            "cleared {gone} Done row{} from the feed",
            if gone == 1 { "" } else { "s" }
        ),
        Err(e) => format!("{}: {e}", file.display()),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The `giverny manage` entrypoint. Returns the process exit code. `spool` is
/// where a message to the app goes when its socket is not there
/// (`clear-done`).
pub fn main(args: &[String], spool: &Path) -> i32 {
    let (cmd, flags) = match parse_args(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if cmd == Cmd::Accuracy {
        // The history is the machine's, not a session's.
        println!("{}", accuracy(&feed::feed_dir(), flags.repo.as_deref()));
        return 0;
    }
    let session = flags
        .session
        .clone()
        .or_else(|| std::env::var(SESSION_ENV).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(session) = session else {
        eprintln!(
            "giverny manage: no session — run it from inside Claude Code \
             (it sets ${SESSION_ENV}) or pass --session <id>"
        );
        return 2;
    };
    // Another live session's rows and leases are its own:
    // `--session` speaks as it only to read.
    if let Some(mine) = std::env::var(SESSION_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && *s != session)
        && cmd.changes()
    {
        let w = crate::reach_guard::System::new(feed::feed_dir());
        if let Some(why) = crate::reach_guard::speak_as(&session, &w.me(&mine), &w) {
            eprintln!("giverny manage: {why}");
            return 1;
        }
    }
    if cmd == Cmd::Watch {
        let dir = feed::feed_dir();
        let transcript = continuation::config_dir()
            .and_then(|c| crate::subagents::session_transcript(&c, &session));
        let look = crate::manage_progress::System {
            feed_dir: &dir,
            session: &session,
            transcript: transcript.as_deref(),
        };
        println!("{}", crate::manage_progress::watch(&dir, &session, &look));
        return 0;
    }
    let now = now_ms();
    let answer = match &cmd {
        Cmd::Claim(task) if flags.wait => claim_wait(
            &feed::feed_dir(),
            &session,
            task,
            &flags,
            None,
            &mut now_ms,
            &mut |ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
            &mut |line| {
                use std::io::Write;
                println!("{line}");
                let _ = std::io::stdout().flush();
            },
        ),
        _ => run_in_code(&feed::feed_dir(), &session, &cmd, &flags, now, None),
    };
    match answer {
        Ok((msg, _)) if cmd == Cmd::ClearDone => {
            let pane = if crate::hooks::send_clear_done(spool, Some(&session), now) {
                "asked Giverny to clear the management panel's Done rows in this tab"
            } else {
                "not in a Giverny tab, so no pane to clear"
            };
            println!("{msg}; {pane}");
            0
        }
        Ok((msg, code)) => {
            if !msg.is_empty() {
                println!("{}", msg.trim_end());
            }
            code
        }
        Err(e) => {
            eprintln!("giverny manage: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    const T0: u64 = 1_790_000_000_000;
    const MIN: u64 = 60_000;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-manage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn run(dir: &Path, line: &str, now: u64) -> Result<String, String> {
        let (cmd, flags) = parse_args(&args(line))?;
        run_in(dir, "s1", &cmd, &flags, now)
    }

    fn read_feed(dir: &Path) -> feed::Feed {
        feed::read(&feed::feed_path(dir, "s1")).expect("a feed the pane reads")
    }

    #[test]
    fn durations() {
        assert_eq!(parse_dur("25"), Some(1500));
        assert_eq!(parse_dur("25m"), Some(1500));
        assert_eq!(parse_dur("1h30m"), Some(5400));
        assert_eq!(parse_dur("1.5h"), Some(5400));
        assert_eq!(parse_dur("~2h"), Some(7200));
        assert_eq!(parse_dur("90s"), Some(90));
        assert_eq!(parse_dur("soon"), None);
        assert_eq!(parse_dur("-5"), None);
        assert_eq!(parse_dur("5x"), None);
    }

    #[test]
    fn a_manager_session_from_plan_to_land_is_what_the_pane_reads() {
        let dir = scratch("life");
        run(&dir, "plan auth-fix --eta 30 --title Fix", T0).unwrap();
        run(&dir, "plan docs --eta 1h", T0).unwrap();
        let f = read_feed(&dir);
        assert_eq!(f.session.as_deref(), Some("s1"));
        assert_eq!(f.rows.len(), 2);
        assert_eq!(f.rows[0].stage(), feed::Stage::Planned);
        assert_eq!(f.rows[0].eta_s, Some(1800));
        assert_eq!(f.rows[0].title.as_deref(), Some("Fix"));
        assert_eq!(f.rows[1].eta_s, Some(3600));

        // Started five minutes later: the clock is the machine's.
        run(&dir, "start auth-fix", T0 + 5 * MIN).unwrap();
        let f = read_feed(&dir);
        assert_eq!(f.rows[0].stage(), feed::Stage::Running);
        assert_eq!(f.rows[0].started_ms, Some(T0 + 5 * MIN));
        assert_eq!(f.rows[0].eta_s, Some(1800), "the planned estimate carries");

        // Ten minutes in, the worker says 40 more: the total is 50.
        run(&dir, "eta auth-fix 40 --note bigger", T0 + 15 * MIN).unwrap();
        let f = read_feed(&dir);
        assert_eq!(f.rows[0].eta_s, Some(50 * 60));
        assert_eq!(f.rows[0].note.as_deref(), Some("bigger"));

        // Lands at 55 minutes: five late against 50.
        run(&dir, "land auth-fix", T0 + 60 * MIN).unwrap();
        let f = read_feed(&dir);
        let r = &f.rows[0];
        assert_eq!(r.stage(), feed::Stage::Done);
        assert_eq!(r.ended_ms, Some(T0 + 60 * MIN));
        assert_eq!(r.landing.as_deref(), Some("Done"));
        let took = (r.ended_ms.unwrap() - r.started_ms.unwrap()) as i64 / 1000;
        assert_eq!(took - r.eta_s.unwrap() as i64, 300);

        // Started again after landing is refused; eta on a landed row too.
        assert!(run(&dir, "start auth-fix", T0 + 61 * MIN).is_err());
        assert!(run(&dir, "eta auth-fix 5", T0 + 61 * MIN).is_err());
        let shown = run(&dir, "show", T0 + 61 * MIN).unwrap();
        assert!(shown.contains("auth-fix"), "{shown}");
        assert!(shown.contains("(+5m)"), "{shown}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `land` and `drop` refuse a task with a `run` command still going,
    /// for every outcome, change nothing and stop nothing; with it done,
    /// `land` goes through. A `start --agent` that would land the worker's
    /// earlier task is refused the same way.
    #[test]
    fn land_drop_and_a_hand_off_refuse_while_a_run_still_runs() {
        let dir = scratch("land-runs");
        run(&dir, "start a --eta 30 --agent w1", T0).unwrap();
        let runs = crate::run_live::runs_dir(&resources::ledger_path(&dir));
        std::fs::create_dir_all(&runs).unwrap();
        let cmd = ["sleep".to_string(), "300".to_string()];
        let live = crate::run_live::register_bound(
            &runs.join("1-0.stats"),
            "a",
            "s1",
            T0 + 2 * MIN,
            None,
            &cmd,
        )
        .expect("registered");
        let me = std::process::id();
        let before = std::fs::read(file_for(&dir, "s1")).unwrap();
        for line in [
            "land a",
            "land a --outcome Blocked --note waiting",
            "land a --review alice — look",
            "drop a",
        ] {
            let e = run(&dir, line, T0 + 10 * MIN).unwrap_err();
            let verb = line.split(' ').next().unwrap();
            assert_eq!(
                e,
                format!(
                    "`a` still runs, so it is not done and does not {verb}: a task with \
                     anything still running is not done. Giverny stops nothing, and nothing \
                     overrides this:\n  1 `giverny-manage run` command still runs:\n    \
                     `sleep 300`, 8m in, pid {me}: `kill {me}` stops it and all it started\n  \
                     Nothing was changed. Once they are stopped or done, run \
                     `giverny-manage {verb} a` again."
                ),
                "{line}"
            );
        }
        assert_eq!(
            std::fs::read(file_for(&dir, "s1")).unwrap(),
            before,
            "nothing changed"
        );
        assert!(crate::run_live::alive(me), "nothing was stopped");
        // Handing w1 on lands a: refused too, and b is not started.
        let e = run(&dir, "start b --eta 30 --agent w1", T0 + 10 * MIN).unwrap_err();
        assert!(
            e.starts_with("handing w1 on to `b` lands `a`, which still runs: "),
            "{e}"
        );
        assert!(
            e.ends_with("run `giverny-manage start b --agent w1` again."),
            "{e}"
        );
        assert_eq!(read_feed(&dir).rows.len(), 1);
        // Another task with nothing running lands.
        run(&dir, "start c --eta 30", T0).unwrap();
        assert!(run(&dir, "land c", T0 + 10 * MIN).is_ok());
        drop(live);
        assert!(
            run(&dir, "start b --eta 30 --agent w1", T0 + 11 * MIN)
                .unwrap()
                .contains("w1 handed on from a")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_lose_no_rows() {
        let dir = scratch("race");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    run(&dir, &format!("plan t{i} --eta 5"), T0).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read_feed(&dir).rows.len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 14 cores, 23 G; limits 12 cores, 16 G; idle.
    fn machine() -> resources::Capacity {
        use giverny_core::limits::{Limits, Load, Machine, Mem, Resolved};
        resources::Capacity {
            machine: Machine {
                cores: 14,
                ram: Mem::gb(23),
                gpus: vec![],
            },
            limits: Resolved {
                cpu_cores: 12,
                ram: Mem::gb(16),
                gpus: vec![],
            },
            configured: Limits::default(),
            load: Load {
                mem_available: Some(Mem::gb(20)),
                load1: Some(0.0),
            },
            default_lease: Default::default(),
            mem_use: Default::default(),
            cpu_use: Default::default(),
            cpu: resources::CpuBusy {
                slice_m: Some(0),
                ..Default::default()
            },
            live_sessions: None,
            run_bounds: Default::default(),
        }
    }

    fn run_as(dir: &Path, session: &str, line: &str, now: u64) -> (String, i32) {
        let (cmd, flags) = parse_args(&args(line)).unwrap();
        run_in_code(dir, session, &cmd, &flags, now, Some(&machine())).unwrap()
    }

    #[test]
    fn claim_queues_past_the_limit_across_sessions_and_land_releases() {
        let dir = scratch("claim");
        run_as(&dir, "a", "start t1 --eta 30", T0);
        let (msg, code) = run_as(&dir, "a", "claim t1 --ram 10G --slot cargo:/t", T0);
        assert_eq!(code, resources::exit::GRANTED, "{msg}");
        assert_eq!(msg, "granted t1: 10G, slot cargo:/t");
        // The row carries its lease for the pane.
        let f = feed::read(&feed::feed_path(&dir, "a")).unwrap();
        let l = f.rows[0].lease.as_ref().unwrap();
        assert_eq!((l.state, l.ram_mb), (feed::LeaseState::Granted, 10240));

        // Session b asks past the limit: queued behind t1, with t1's ETA.
        run_as(&dir, "b", "plan t2 --eta 10", T0);
        let (msg, code) = run_as(&dir, "b", "claim t2 --ram 6G", T0 + 16 * MIN);
        assert_eq!(code, resources::exit::QUEUED, "{msg}");
        assert!(
            msg.starts_with("queued #1 for 6G behind t1 (10G, slot cargo:/t; ~14m)"),
            "{msg}"
        );
        let f = feed::read(&feed::feed_path(&dir, "b")).unwrap();
        let l = f.rows[0].lease.as_ref().unwrap();
        assert_eq!(l.state, feed::LeaseState::Queued);
        assert_eq!((l.position, l.behind.as_deref()), (Some(1), Some("t1")));
        assert_eq!(
            l.short,
            vec!["ram".to_string()],
            "the pane says what the line says"
        );
        // The slot is exclusive whatever the size.
        let (_, code) = run_as(&dir, "b", "claim t3 --slot cargo:/t", T0 + 16 * MIN);
        assert_eq!(code, resources::exit::QUEUED);
        run_as(&dir, "b", "release t3", T0 + 16 * MIN);

        let shown = run_as(&dir, "b", "resources", T0 + 17 * MIN).0;
        assert!(
            shown.contains("limits    12 cores (auto), 16G RAM (auto)"),
            "{shown}"
        );
        assert!(shown.contains("t1"), "{shown}");
        assert!(shown.contains("#1 t2"), "{shown}");
        assert!(
            shown.contains("cpu       0.0 of 12 busy (giverny-claude.slice, last minute) — shared, not reserved; claims go on"),
            "{shown}"
        );
        assert!(!shown.contains(" cpu,"), "no cores leased: {shown}");

        // t1 lands: its lease goes, and t2's re-claim is granted.
        let (msg, _) = run_as(&dir, "a", "land t1", T0 + 18 * MIN);
        assert!(msg.contains("released 10G, slot cargo:/t"), "{msg}");
        let f = feed::read(&feed::feed_path(&dir, "a")).unwrap();
        assert_eq!(f.rows[0].lease, None, "a landed row holds nothing");
        let (msg, code) = run_as(&dir, "b", "claim t2 --ram 6G", T0 + 19 * MIN);
        assert_eq!(code, resources::exit::GRANTED, "{msg}");
        // Too big for the limits is refused, not queued.
        let (msg, code) = run_as(&dir, "a", "claim huge --ram 20G", T0 + 19 * MIN);
        assert_eq!(code, resources::exit::REFUSED, "{msg}");
        // `drop` releases too; `release` of nothing says so.
        let (msg, _) = run_as(&dir, "b", "drop t2", T0 + 22 * MIN);
        assert!(msg.contains("released"), "{msg}");
        assert_eq!(
            run_as(&dir, "b", "release t2", T0 + 22 * MIN).0,
            "t2 held no lease"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every ledger and row command is the caller's own
    /// session's: another's same-named task is never touched, and a release
    /// of it says whose it is and how to ask.
    #[test]
    fn a_session_never_releases_or_lands_another_sessions_task() {
        let dir = scratch("reach");
        run_as(&dir, "sess-a", "start scale343 --eta 60", T0);
        let (msg, _) = run_as(&dir, "sess-a", "claim scale343 --ram 6G", T0);
        assert!(msg.starts_with("granted"), "{msg}");
        let said = run_as(&dir, "sess-b", "release scale343", T0 + MIN).0;
        assert!(
            said.starts_with("scale343 held no lease of this session's"),
            "{said}"
        );
        assert!(said.contains("another session (sess-a) holds"), "{said}");
        assert!(said.contains("`giverny-manage ask scale343 "), "{said}");
        for line in [
            "land scale343",
            "drop scale343",
            "pause scale343",
            "eta scale343 5",
        ] {
            let _ = run_line(&dir, "sess-b", line, T0 + MIN);
        }
        let l = crate::manage_inbox::read_ledger(&resources::ledger_path(&dir), T0 + MIN).unwrap();
        assert!(
            l.leases
                .iter()
                .any(|x| x.session == "sess-a" && x.task == "scale343"),
            "a's lease stands"
        );
        let f = feed::read(&feed::feed_path(&dir, "sess-a")).unwrap();
        assert_eq!(f.rows[0].stage(), feed::Stage::Running, "a's row stands");
        // Its own release says nothing of anyone else.
        assert!(
            run_as(&dir, "sess-a", "release scale343", T0 + MIN)
                .0
                .starts_with("released")
        );
        assert_eq!(
            run_as(&dir, "sess-a", "release scale343", T0 + MIN).0,
            "scale343 held no lease"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn line(s: &str) -> Vec<String> {
        // Like a shell: `"…"` is one argument.
        let mut out = Vec::new();
        for (i, part) in s.split('"').enumerate() {
            if i % 2 == 1 {
                out.push(part.to_string());
            } else {
                out.extend(part.split_whitespace().map(String::from));
            }
        }
        out
    }

    fn run_line(dir: &Path, session: &str, l: &str, now: u64) -> Result<(String, i32), String> {
        let (cmd, flags) = parse_args(&line(l))?;
        run_in_code(dir, session, &cmd, &flags, now, Some(&machine()))
    }
}
