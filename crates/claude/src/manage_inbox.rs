//! `giverny manage ask` / `reply`: managers talk when the ledger cannot
//! grant.
//!
//! The ledger ([`crate::resources`]) answers *queued behind demo#12*; it
//! cannot say whether demo#12's manager would hand over its cargo
//! slot for a two-minute test of a higher-Priority task. So the queued one
//! asks: `giverny manage ask demo#12 "<why>"` finds the session holding that
//! lease and drops a message in that session's **inbox**, a JSON-lines file
//! at `<feed dir>/inbox/<session>.jsonl`. The plugin's `PostToolUse` hook
//! (`giverny hook`) checks the calling session's inbox on every tool
//! call — one `stat` when it is empty — and hands what it finds to the
//! session as `additionalContext`, with who asks, what they hold and want,
//! their Priority and ETA, and the commands to answer: `reply`, `release`, or
//! `claim --ram …` to shrink in place. The reply comes back to the asker the
//! same way.
//!
//! A delivered message moves to `<session>.seen.jsonl` (kept a day), where
//! `reply <id>` finds it. A message lives as long as the asker's ledger entry
//! for its task (a lease or a place in the queue); one sent by a session that
//! holds nothing lives [`resources::TTL_MS`].

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::feed;
use crate::manage::{self, Lock};
use crate::resources::{self, Ledger};

/// Where the inboxes live, under the feed directory.
pub const INBOX_DIR: &str = "inbox";
/// How long a delivered message is kept for `reply` to find.
const SEEN_KEEP_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Ask,
    Reply,
}

/// The ledger entry (lease or place in the queue) whose life is the
/// message's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub session: String,
    pub task: String,
}

/// One message, a line of an inbox file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub kind: Kind,
    #[serde(with = "resources::ts")]
    pub at: u64,
    pub from_session: String,
    /// The sender's task: the asker's own, or for a reply the task asked
    /// about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_task: Option<String>,
    pub to_session: String,
    /// The recipient's task the ask is about (its lease), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// What the sender holds for `from_task` (`3G, slot cargo:/t`), or `released`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holds: Option<String>,
    /// What the sender waits for in the queue, and where.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wants: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// The sender's task's time left (running) or estimate (planned), s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_s: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_with: Option<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
}

pub fn inbox_dir(feed_dir: &Path) -> PathBuf {
    feed_dir.join(INBOX_DIR)
}

/// `<feed dir>/inbox/<session>.jsonl`: unread messages to `session`.
pub fn inbox_path(feed_dir: &Path, session: &str) -> PathBuf {
    inbox_dir(feed_dir).join(format!("{session}.jsonl"))
}

/// `<feed dir>/inbox/<session>.seen.jsonl`: delivered ones, for `reply`.
fn seen_path(feed_dir: &Path, session: &str) -> PathBuf {
    inbox_dir(feed_dir).join(format!("{session}.seen.jsonl"))
}

/// A short id: `m` and six base-36 characters.
fn new_id(now: u64) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut x = now
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((std::process::id() as u64) << 20)
        .wrapping_add(N.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x2545_F491));
    x ^= x >> 29;
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = String::from("m");
    for _ in 0..6 {
        s.push(digits[(x % 36) as usize] as char);
        x /= 36;
    }
    s
}

fn read_lines(path: &Path) -> Vec<Message> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn append(path: &Path, msgs: &[Message]) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = String::new();
    for m in msgs {
        buf.push_str(&serde_json::to_string(m).map_err(|e| e.to_string())?);
        buf.push('\n');
    }
    f.write_all(buf.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Drop `msg` in its recipient's inbox.
pub fn post(feed_dir: &Path, msg: &Message) -> Result<(), String> {
    let path = inbox_path(feed_dir, &msg.to_session);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let _lock = Lock::take(&path)?;
    append(&path, std::slice::from_ref(msg))
}

/// The ledger as it is now, read without its lock (writes are atomic
/// renames), expired entries dropped. `None` when there is none.
pub fn read_ledger(path: &Path, now: u64) -> Option<Ledger> {
    let mut l = Ledger::parse(&std::fs::read(path).ok()?).ok()?;
    l.expire(now);
    Some(l)
}

fn has_entry(l: &Ledger, session: &str, task: &str) -> bool {
    l.leases
        .iter()
        .any(|x| x.session == session && x.task == task)
        || l.queue
            .iter()
            .any(|w| w.session == session && w.task == task)
}

/// Is `m` still worth delivering: its asker still holds or waits for its
/// task, or (an asker with no entry) it is younger than the lease TTL.
pub fn is_live(m: &Message, ledger: Option<&Ledger>, now: u64) -> bool {
    match &m.expires_with {
        Some(e) => ledger.is_some_and(|l| has_entry(l, &e.session, &e.task)),
        None => m.at.saturating_add(resources::TTL_MS) > now,
    }
}

/// Take `session`'s unread messages: the live ones are returned, every one
/// moves to the seen file. One `stat` when there are none.
pub fn take(feed_dir: &Path, ledger: &Path, session: &str, now: u64) -> Vec<Message> {
    let path = inbox_path(feed_dir, session);
    if std::fs::metadata(&path).is_err() {
        return Vec::new();
    }
    let Ok(_lock) = Lock::take(&path) else {
        return Vec::new();
    };
    let msgs = read_lines(&path);
    let _ = std::fs::remove_file(&path);
    if msgs.is_empty() {
        return msgs;
    }
    let l = read_ledger(ledger, now);
    let stamp = manage::stamp(now);
    let (live, dead): (Vec<_>, Vec<_>) =
        msgs.into_iter().partition(|m| is_live(m, l.as_ref(), now));
    let seen = seen_path(feed_dir, session);
    let mut kept: Vec<Message> = read_lines(&seen)
        .into_iter()
        .filter(|m| m.at.saturating_add(SEEN_KEEP_MS) > now)
        .collect();
    kept.extend(live.iter().cloned().map(|mut m| {
        m.delivered_at = Some(stamp.clone());
        m
    }));
    kept.extend(dead.into_iter().map(|mut m| {
        m.delivered_at = Some("expired".into());
        m
    }));
    let tmp = seen.with_extension(format!("jsonl.{}.tmp", std::process::id()));
    if append(&tmp, &kept).is_ok() {
        let _ = std::fs::rename(&tmp, &seen);
    }
    live
}

/// Find message `id`: in `session`'s own inbox files first, then anyone's.
pub fn find(feed_dir: &Path, session: &str, id: &str) -> Option<Message> {
    let hit = |p: &Path| read_lines(p).into_iter().rev().find(|m| m.id == id);
    hit(&seen_path(feed_dir, session))
        .or_else(|| hit(&inbox_path(feed_dir, session)))
        .or_else(|| {
            std::fs::read_dir(inbox_dir(feed_dir))
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .find_map(|p| hit(&p))
        })
}

/// Whom `target` names: a task holding a lease (else waiting in the queue)
/// in another session, or a session id (or an unambiguous prefix of one).
/// The session and the task asked about.
pub fn resolve(l: &Ledger, me: &str, target: &str) -> Result<(String, Option<String>), String> {
    let target = target.trim();
    let mut by_task: Vec<(String, String)> = l
        .leases
        .iter()
        .filter(|x| x.task == target)
        .map(|x| (x.session.clone(), x.task.clone()))
        .collect();
    if by_task.is_empty() {
        by_task = l
            .queue
            .iter()
            .filter(|w| w.task == target)
            .map(|w| (w.session.clone(), w.task.clone()))
            .collect();
    }
    let others: Vec<&(String, String)> = by_task.iter().filter(|(s, _)| s != me).collect();
    match (others.as_slice(), by_task.is_empty()) {
        ([one], _) => return Ok((one.0.clone(), Some(one.1.clone()))),
        ([], false) => return Err(format!("`{target}` is this session's own task")),
        ([], true) => {}
        (many, _) => {
            let ss: Vec<&str> = many.iter().map(|(s, _)| short(s)).collect();
            return Err(format!(
                "`{target}` is held in {} sessions ({}); ask one by its session id",
                many.len(),
                ss.join(", ")
            ));
        }
    }
    let mut sessions: Vec<&str> = l
        .leases
        .iter()
        .map(|x| x.session.as_str())
        .chain(l.queue.iter().map(|w| w.session.as_str()))
        .filter(|s| *s == target || (target.len() >= 6 && s.starts_with(target)))
        .collect();
    sessions.sort();
    sessions.dedup();
    let session = match sessions.as_slice() {
        [one] => one.to_string(),
        [] if looks_like_session(target) => target.to_string(),
        [] => {
            return Err(format!(
                "no lease or queued task named `{target}` in the ledger \
                 (`giverny manage resources` lists them), and it is no session id"
            ));
        }
        _ => return Err(format!("`{target}` is the start of several session ids")),
    };
    if session == me {
        return Err("that is this session".into());
    }
    let held: Vec<&str> = l
        .leases
        .iter()
        .filter(|x| x.session == session)
        .map(|x| x.task.as_str())
        .collect();
    let about = match held.as_slice() {
        [one] => Some(one.to_string()),
        _ => None,
    };
    Ok((session, about))
}

fn looks_like_session(s: &str) -> bool {
    s.len() >= 8
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn short(s: &str) -> &str {
    s.get(..8).unwrap_or(s)
}

/// The asker's task when it does not say: its one place in the queue, else
/// its one lease.
fn own_task(l: &Ledger, me: &str) -> Option<String> {
    let q: Vec<&str> = l
        .queue
        .iter()
        .filter(|w| w.session == me)
        .map(|w| w.task.as_str())
        .collect();
    if let [one] = q.as_slice() {
        return Some(one.to_string());
    }
    let h: Vec<&str> = l
        .leases
        .iter()
        .filter(|x| x.session == me)
        .map(|x| x.task.as_str())
        .collect();
    match h.as_slice() {
        [one] => Some(one.to_string()),
        _ => None,
    }
}

/// `giverny manage ask <task-or-session> "<msg>" [--task <mine>] [--priority P]`.
#[allow(clippy::too_many_arguments)]
pub fn ask(
    feed_dir: &Path,
    ledger: &Path,
    me: &str,
    target: &str,
    text: &str,
    my_task: Option<&str>,
    priority: Option<&str>,
    now: u64,
) -> Result<String, String> {
    let l = read_ledger(ledger, now).unwrap_or_default();
    let (to, about) = resolve(&l, me, target)?;
    let task = my_task.map(String::from).or_else(|| own_task(&l, me));
    let lease = task.as_deref().and_then(|t| l.lease(me, t));
    let order = l.ordered_queue();
    let waiter = task
        .as_deref()
        .and_then(|t| order.iter().position(|w| w.session == me && w.task == t));
    let msg = Message {
        id: new_id(now),
        kind: Kind::Ask,
        at: now,
        from_session: me.into(),
        from_task: task.clone(),
        to_session: to.clone(),
        about: about.clone(),
        text: text.trim().into(),
        in_reply_to: None,
        holds: lease.map(|x| x.describe()),
        wants: waiter.map(|i| order[i].request.describe()),
        position: waiter.map(|i| i + 1),
        priority: priority
            .map(String::from)
            .or_else(|| waiter.and_then(|i| order[i].request.priority.clone())),
        eta_s: task
            .as_deref()
            .and_then(|t| resources::eta_or_estimate_s(feed_dir, me, t, now)),
        expires_with: task
            .as_deref()
            .filter(|t| has_entry(&l, me, t))
            .map(|t| Entry {
                session: me.into(),
                task: t.into(),
            }),
        delivered_at: None,
    };
    post(feed_dir, &msg)?;
    let whom = match &about {
        Some(t) => format!("{t} (session {})", short(&to)),
        None => format!("session {}", short(&to)),
    };
    let life = match &msg.expires_with {
        Some(e) => format!("while {} holds or waits", e.task),
        None => format!(
            "for {} (this session holds nothing in the ledger)",
            feed::fmt_span((resources::TTL_MS / 1000) as i64)
        ),
    };
    Ok(format!(
        "asked {whom}: message {}. It reaches that manager on its next tool call, \
         and its reply reaches this session the same way; the message lives {life}",
        msg.id
    ))
}

/// `giverny manage reply <msg-id> "<text>"`.
pub fn reply(
    feed_dir: &Path,
    ledger: &Path,
    me: &str,
    id: &str,
    text: &str,
    now: u64,
) -> Result<String, String> {
    let orig = find(feed_dir, me, id).ok_or_else(|| {
        format!("no message {id} in this session's inbox (or anyone's) to reply to")
    })?;
    if orig.from_session == me {
        return Err(format!("{id} is this session's own message"));
    }
    let l = read_ledger(ledger, now);
    let about = orig.about.clone();
    let holds = about.as_deref().map(|t| {
        l.as_ref()
            .and_then(|l| l.lease(me, t))
            .map(|x| x.describe())
            .unwrap_or_else(|| "released".into())
    });
    let msg = Message {
        id: new_id(now),
        kind: Kind::Reply,
        at: now,
        from_session: me.into(),
        from_task: about.clone(),
        to_session: orig.from_session.clone(),
        about: orig.from_task.clone(),
        text: text.trim().into(),
        in_reply_to: Some(orig.id.clone()),
        holds,
        wants: None,
        position: None,
        priority: None,
        eta_s: about
            .as_deref()
            .and_then(|t| resources::eta_or_estimate_s(feed_dir, me, t, now)),
        expires_with: orig.expires_with.clone(),
        delivered_at: None,
    };
    let live = is_live(&msg, l.as_ref(), now);
    post(feed_dir, &msg)?;
    let whom = match &orig.from_task {
        Some(t) => format!("{t} (session {})", short(&orig.from_session)),
        None => format!("session {}", short(&orig.from_session)),
    };
    Ok(if live {
        format!("replied to {whom}: message {}", msg.id)
    } else {
        format!(
            "replied to {whom}: message {}; but the asker no longer holds or waits for \
             its task, so it will not be delivered",
            msg.id
        )
    })
}

fn ago(now: u64, at: u64) -> String {
    let s = now.saturating_sub(at) / 1000;
    if s < 60 {
        "just now".into()
    } else {
        format!("{} ago", feed::fmt_span(s as i64))
    }
}

/// What the sender is: `task acme#5 — priority high, queued #1 for 8G,
/// ~10m of work`.
fn sender(m: &Message) -> String {
    let mut s = format!("session {}", short(&m.from_session));
    if let Some(t) = &m.from_task {
        s.push_str(&format!(", task {t}"));
    }
    let mut bits = Vec::new();
    if let Some(p) = &m.priority {
        bits.push(format!("priority {p}"));
    }
    if let (Some(w), Some(p)) = (&m.wants, m.position) {
        bits.push(format!("queued #{p} for {w}"));
    }
    if let Some(h) = m.holds.as_ref().filter(|_| m.kind == Kind::Ask) {
        bits.push(format!("holds {h}"));
    }
    if let Some(e) = m.eta_s {
        bits.push(format!("~{} left on it", feed::fmt_span(e.max(0))));
    }
    if !bits.is_empty() {
        s.push_str(&format!(" ({})", bits.join(", ")));
    }
    s
}

/// The hook's text for one message to `me`, given the ledger now.
pub fn render(m: &Message, me: &str, ledger: Option<&Ledger>, now: u64) -> String {
    match m.kind {
        Kind::Ask => {
            let mine: Vec<&resources::Lease> = ledger
                .map(|l| l.leases.iter().filter(|x| x.session == me).collect())
                .unwrap_or_default();
            let only = match mine.as_slice() {
                [one] => Some(one.task.as_str()),
                _ => None,
            };
            let about = m.about.as_deref().or(only);
            let lease_line = match about {
                Some(t) => match mine.iter().find(|x| x.task == t) {
                    Some(x) => format!("It asks about your lease {t} ({}).", x.describe()),
                    None => format!("It asks about {t}, which holds no lease now."),
                },
                None if mine.is_empty() => "You hold no lease now.".to_string(),
                None => format!(
                    "You hold: {}.",
                    mine.iter()
                        .map(|x| format!("{} ({})", x.task, x.describe()))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            };
            let t = about.unwrap_or("<task>");
            format!(
                "Giverny: another manager on this machine asks for resources \
                 (message {id}, {ago}). From {from}. {lease_line} It says: \"{text}\"\n\
                 Weigh its Priority and time left against yours. If you give way, do it first:\n\
                 - free it: `giverny-manage release {t}` (a worker's next `giverny-manage run {t}` \
                 claims afresh and waits its turn)\n\
                 - or shrink it in place: `giverny-manage claim {t} --ram <less>`\n\
                 Then always answer, even to say no: `giverny-manage reply {id} \"<your answer>\"`, \
                 and carry on.",
                id = m.id,
                ago = ago(now, m.at),
                from = sender(m),
                text = m.text,
            )
        }
        Kind::Reply => {
            // What the replier holds now, by the ledger, over what it held
            // when it replied: a holder may answer before it releases.
            let now_holds = match (ledger, &m.from_task) {
                (Some(l), Some(t)) => Some(
                    l.lease(&m.from_session, t)
                        .map(|x| x.describe())
                        .unwrap_or_else(|| "released".into()),
                ),
                _ => m.holds.clone(),
            };
            let state = match (&m.from_task, &now_holds) {
                (Some(t), Some(h)) if h == "released" => format!(" {t} has released its lease."),
                (Some(t), Some(h)) => format!(" {t} now holds {h}."),
                _ => String::new(),
            };
            let retry = match &m.about {
                Some(t) => format!(
                    " If it freed what you wait for, re-run your claim now \
                     (`giverny-manage claim {t} …`, the same flags)."
                ),
                None => String::new(),
            };
            format!(
                "Giverny: reply {id} to your ask {orig}, from {from}, {ago}:{state} \
                 It says: \"{text}\"{retry}",
                id = m.id,
                orig = m.in_reply_to.as_deref().unwrap_or("?"),
                from = sender(m),
                ago = ago(now, m.at),
                text = m.text,
            )
        }
    }
}

/// The hook's delivery: `session`'s unread messages as one text, if any.
pub fn deliver(feed_dir: &Path, ledger: &Path, session: &str, now: u64) -> Option<String> {
    let msgs = take(feed_dir, ledger, session, now);
    if msgs.is_empty() {
        return None;
    }
    let l = read_ledger(ledger, now);
    Some(
        msgs.iter()
            .map(|m| render(m, session, l.as_ref(), now))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}
