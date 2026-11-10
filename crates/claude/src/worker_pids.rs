//! Which worker started which process.
//!
//! Every Bash command a Claude Code session runs, its own or any of its
//! subagents', is a child of the one `claude` process:
//!
//! ```text
//! /bin/bash -c source <snapshot> … && eval '<command>' < /dev/null && pwd -P >| /tmp/claude-…-cwd
//! ```
//!
//! so the process tree alone cannot say which worker a build belongs to.
//! The command text can: a worker's transcript
//! (`<session>/subagents/agent-<id>.jsonl`) has the Bash `tool_use` with
//! that exact `command`, and it is in flight — no `tool_result` yet — or
//! running in the background. When exactly one worker has it in flight
//! (and the session itself does not), the bash process and everything
//! under it are that worker's. Anything else — the session's own commands,
//! the same command in flight in two workers — stays with the session,
//! never guessed.
//!
//! A pid's match never changes, so it is made once, when the process is
//! first seen ([`Attributor`]); transcripts are read incrementally, only
//! for those.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::session_use::{Proc, subtree};

/// The command Claude Code's Bash wrapper evals, out of the wrapper's `-c`
/// script: the single shell word after `&& eval `, unquoted, when the
/// script ends the way the wrapper's does (`< /dev/null && pwd -P …`).
pub fn eval_command(script: &str) -> Option<String> {
    let at = script.find("&& eval ")? + "&& eval ".len();
    let (word, rest) = shell_word(&script[at..])?;
    rest.trim_start().starts_with("< /dev/null").then_some(word)
}

/// One shell word from the start of `s` — `'…'`, `"…"` (with `\"`, `\\`,
/// `\$`, `` \` `` escapes) and bare characters, run together — and what
/// follows it. `None` for an unterminated quote.
fn shell_word(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        match c {
            '\'' => {
                chars.next();
                loop {
                    match chars.next()? {
                        (_, '\'') => break,
                        (_, c) => out.push(c),
                    }
                }
            }
            '"' => {
                chars.next();
                loop {
                    match chars.next()? {
                        (_, '"') => break,
                        (_, '\\') => match chars.next()? {
                            (_, c @ ('"' | '\\' | '$' | '`')) => out.push(c),
                            (_, '\n') => {}
                            (_, c) => {
                                out.push('\\');
                                out.push(c);
                            }
                        },
                        (_, c) => out.push(c),
                    }
                }
            }
            '\\' => {
                chars.next();
                if let Some((_, c)) = chars.next() {
                    out.push(c);
                }
            }
            c if c.is_whitespace() => return Some((out, &s[i..])),
            c => {
                chars.next();
                out.push(c);
            }
        }
    }
    Some((out, ""))
}

/// Who wrote a `tool_use`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Who {
    /// The session's own transcript.
    Session,
    /// A worker, by agent id.
    Agent(String),
}

#[derive(Debug, Clone)]
struct Call {
    who: Who,
    id: String,
    background: bool,
}

/// Every Bash `tool_use` in a session's transcripts, by command, and the
/// ids that have their `tool_result`. Fed incrementally: each file is read
/// from where the last read stopped.
#[derive(Debug, Default)]
pub struct CommandIndex {
    read: HashMap<PathBuf, (u64, Vec<u8>)>,
    calls: HashMap<String, Vec<Call>>,
    done: HashSet<String>,
}

impl CommandIndex {
    /// Read what `path` (written by `who`) gained since the last read.
    pub fn update(&mut self, path: &Path, who: &Who) {
        let (offset, partial) = self.read.entry(path.to_path_buf()).or_default();
        let Ok(mut f) = std::fs::File::open(path) else {
            return;
        };
        if f.seek(SeekFrom::Start(*offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return;
        }
        *offset += buf.len() as u64;
        partial.extend_from_slice(&buf);
        let Some(end) = partial.iter().rposition(|b| *b == b'\n') else {
            return;
        };
        let lines: Vec<u8> = partial.drain(..=end).collect();
        for line in lines.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            self.line(line, who);
        }
    }

    /// One transcript line.
    pub fn line(&mut self, line: &[u8], who: &Who) {
        // Most lines are neither; skip them unparsed.
        let has = |pat: &[u8]| line.windows(pat.len()).any(|w| w == pat);
        if !has(b"\"tool_use\"") && !has(b"\"tool_result\"") {
            return;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            return;
        };
        let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        for c in content {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") if c.get("name").and_then(|n| n.as_str()) == Some("Bash") => {
                    let input = c.get("input");
                    let (Some(cmd), Some(id)) = (
                        input
                            .and_then(|i| i.get("command"))
                            .and_then(|c| c.as_str()),
                        c.get("id").and_then(|i| i.as_str()),
                    ) else {
                        continue;
                    };
                    let background = input
                        .and_then(|i| i.get("run_in_background"))
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    self.calls.entry(cmd.to_string()).or_default().push(Call {
                        who: who.clone(),
                        id: id.to_string(),
                        background,
                    });
                }
                Some("tool_result") => {
                    if let Some(id) = c.get("tool_use_id").and_then(|i| i.as_str()) {
                        self.done.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// The worker whose call `command` is: the one agent among the calls
    /// with that command still in flight or in the background. `None` when
    /// there is none, the session's own is among them, or two workers are.
    pub fn worker_of(&self, command: &str) -> Option<String> {
        let mut who: Option<&Who> = None;
        for c in self.calls.get(command)? {
            if !c.background && self.done.contains(&c.id) {
                continue;
            }
            match who {
                None => who = Some(&c.who),
                Some(w) if *w == c.who => {}
                Some(_) => return None,
            }
        }
        match who? {
            Who::Agent(id) => Some(id.clone()),
            Who::Session => None,
        }
    }
}

/// How many passes a command that matches no worker yet is tried again
/// (its `tool_use` may not be written yet) before it is the session's.
const TRIES: u8 = 3;

/// Keeps every bash process's worker, once found, and each session's
/// [`CommandIndex`].
#[derive(Debug, Default)]
pub struct Attributor {
    /// `(pid, start)` → the worker, or `None` once given up on.
    known: HashMap<(u32, u64), Option<String>>,
    tries: HashMap<(u32, u64), u8>,
    sessions: HashMap<String, CommandIndex>,
}

/// What [`Attributor::attribute`] needs from the machine, so tests can
/// stand in for `/proc` and `~/.claude`.
pub trait Source {
    /// The bash wrapper's `-c` script, if `pid` is one.
    fn wrapper_script(&self, pid: u32) -> Option<String>;
    /// The session id and the Claude config dir from `pid`'s environment.
    fn session_of(&self, pid: u32) -> Option<(String, Option<PathBuf>)>;
    /// The session's transcripts: its own, and each worker's by agent id.
    fn transcripts(&self, session: &str, config_dir: Option<&Path>) -> Vec<(Who, PathBuf)>;
}

impl Attributor {
    /// Each worker's processes: every child of a claude in `claudes` that
    /// is a Bash wrapper matched to a worker, with all under it.
    pub fn attribute(
        &mut self,
        procs: &[Proc],
        claudes: &HashSet<u32>,
        src: &impl Source,
    ) -> HashMap<String, HashSet<u32>> {
        let alive: HashSet<(u32, u64)> = procs.iter().map(|p| (p.pid, p.start)).collect();
        self.known.retain(|k, _| alive.contains(k));
        self.tries.retain(|k, _| alive.contains(k));
        let mut out: HashMap<String, HashSet<u32>> = HashMap::new();
        for p in procs.iter().filter(|p| claudes.contains(&p.ppid)) {
            let key = (p.pid, p.start);
            let worker = match self.known.get(&key) {
                Some(w) => w.clone(),
                None => {
                    let found = self.find(p.pid, src);
                    if found.is_some() {
                        self.known.insert(key, found.clone());
                    } else {
                        let n = self.tries.entry(key).or_default();
                        *n += 1;
                        if *n >= TRIES {
                            self.known.insert(key, None);
                        }
                    }
                    found
                }
            };
            if let Some(w) = worker {
                out.entry(w).or_default().extend(subtree(procs, p.pid));
            }
        }
        out
    }

    fn find(&mut self, pid: u32, src: &impl Source) -> Option<String> {
        let command = eval_command(&src.wrapper_script(pid)?)?;
        let (session, config_dir) = src.session_of(pid)?;
        let index = self.sessions.entry(session.clone()).or_default();
        for (who, path) in src.transcripts(&session, config_dir.as_deref()) {
            index.update(&path, &who);
        }
        index.worker_of(&command)
    }
}

/// The real machine: `/proc` and the session's transcripts on disk.
pub struct Machine;

impl Source for Machine {
    fn wrapper_script(&self, pid: u32) -> Option<String> {
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let mut argv = cmd.split(|b| *b == 0);
        let bin = String::from_utf8_lossy(argv.next()?).into_owned();
        if !(bin.ends_with("/bash") || bin == "bash") || argv.next()? != b"-c" {
            return None;
        }
        Some(String::from_utf8_lossy(argv.next()?).into_owned())
    }

    fn session_of(&self, pid: u32) -> Option<(String, Option<PathBuf>)> {
        let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let var = |name: &[u8]| {
            env.split(|b| *b == 0)
                .find_map(|kv| kv.strip_prefix(name)?.strip_prefix(b"="))
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .filter(|v| !v.is_empty())
        };
        Some((
            var(b"CLAUDE_CODE_SESSION_ID")?,
            var(b"CLAUDE_CONFIG_DIR").map(PathBuf::from),
        ))
    }

    fn transcripts(&self, session: &str, config_dir: Option<&Path>) -> Vec<(Who, PathBuf)> {
        let cfg = config_dir
            .map(Path::to_path_buf)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")));
        let Some(cfg) = cfg else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for project in std::fs::read_dir(cfg.join("projects"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let own = project.path().join(format!("{session}.jsonl"));
            if own.is_file() {
                out.push((Who::Session, own));
            }
            let subagents = project.path().join(session).join("subagents");
            for id in crate::subagents::list_agent_ids(&subagents) {
                out.push((
                    Who::Agent(id.clone()),
                    crate::subagents::agent_transcript(&subagents, &id),
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRAP: &str = "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && shopt -u extglob 2>/dev/null || true && { \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; } >/dev/null 2>&1 || true && eval ";

    fn wrapped(quoted: &str) -> String {
        format!("{WRAP}{quoted} < /dev/null && pwd -P >| /tmp/claude-e170-cwd")
    }

    #[test]
    fn the_command_comes_out_of_the_wrapper_as_written() {
        // As seen on a live session: single quotes, `'"'"'` for a quote.
        let s = wrapped(
            r#"'until grep -q "peak" /tmp/x.out; do sleep 10; done; tail -n 12 /tmp/x.out | tr '"'"'\r'"'"' '"'"'\n'"'"''"#,
        );
        assert_eq!(
            eval_command(&s).as_deref(),
            Some(
                r#"until grep -q "peak" /tmp/x.out; do sleep 10; done; tail -n 12 /tmp/x.out | tr '\r' '\n'"#
            )
        );
        // Several lines.
        let s = wrapped("'cd /a\ncargo build \\\n  --release'");
        assert_eq!(
            eval_command(&s).as_deref(),
            Some("cd /a\ncargo build \\\n  --release")
        );
        // Not the wrapper's shape: nothing.
        assert_eq!(eval_command("echo hi"), None);
        assert_eq!(eval_command(&format!("{WRAP}'unterminated")), None);
        assert_eq!(eval_command(&format!("{WRAP}'x' | cat")), None);
    }
}
