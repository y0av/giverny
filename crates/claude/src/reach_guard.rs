//! One manager session never reaches into another's processes, run scopes,
//! ledger rows or worktrees.
//!
//! On 2026-10-15 a dispatcher killed another manager's orphaned test worker
//! to free the ledger, unasked. Asking is the one sanctioned channel
//! (`giverny-manage ask <task> "…"`, [`crate::manage_inbox`]); this is the
//! tool refusing the rest, with the owner named and that command said:
//!
//! - **Processes and scopes.** `kill`, `pkill`, `killall` (and `kill
//!   $(pgrep …)`, `pgrep … | xargs kill`) aimed at a process another
//!   session owns, and `systemctl --user stop|kill|restart|freeze` of a
//!   `giverny-run-*` or `giverny-claude-*` unit (or the slice holding them)
//!   another session owns. A process's owner is read from its cgroup — the
//!   tab's scope `giverny-claude-<claude pid>.scope` ([`crate::tab_cap`]),
//!   or a run's `giverny-run-<task>-<run pid>.scope`, whose `.live` record
//!   names its session and task ([`crate::run_live`]) — which an orphan
//!   keeps when its parent is gone; else from the claude among its
//!   parents. The claude's session is Claude Code's own registry
//!   (`<config>/sessions/<pid>.json`, [`crate::registry`]).
//! - **Worktrees.** A mutating git command (checkout/switch, reset,
//!   restore, stash, clean, commit, merge, rebase, …, `worktree
//!   remove|move`, `branch -D`) and an Edit/Write in a linked worktree
//!   another session's live worker works in: the cwd of its live `giverny
//!   manage run` command, or of another live claude started there. The main
//!   checkout is everyone's and never held.
//! - **The ledger.** Every `giverny-manage` subcommand acts on its caller's
//!   own session's rows and leases (a lease is `<session>:<task>`), so the
//!   only way into another's is to speak as it: `--session <id>` (refused in
//!   [`crate::manage`] when that session runs in another live claude) or
//!   `CLAUDE_CODE_SESSION_ID=<id> giverny-manage …` (refused here).
//!
//! **Best effort, never in the way.** The shell is read the way
//! [`crate::bash_guard`] reads it. A target that cannot be resolved — a pid
//! held in a variable, a pattern with regex syntax beyond `.`, `.*`, `^`,
//! `$`, a process with no claude above it, an owner whose claude is gone —
//! is allowed. Only Linux's `/proc` is read; elsewhere nothing is refused.
//! **Cheap:** text only, unless the command kills, stops a unit, mutates
//! git in a linked worktree, or speaks as another session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::bash_guard::{
    LEADERS, MAX_DEPTH, after_nice, after_options, base, is_assignment, split,
};
use crate::manage::SESSION_ENV;

/// What a command reaches for, read from its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// `kill` of these pids (a negative one, a process group, stands for
    /// its leader; `-1` for every process).
    Pids(Vec<i64>),
    /// `pkill`, `killall`, `kill $(pgrep …)`: every process a pattern
    /// matches.
    Match(Pattern),
    /// `systemctl stop|kill|…` of these units.
    Units(Vec<String>),
    /// A mutating git command (`what`) run in `dir` (`None` when it changes
    /// no worktree of its own: `worktree remove`, `branch -D`), naming
    /// worktrees (`paths`) and branches (`branches`).
    Git {
        what: String,
        dir: Option<PathBuf>,
        paths: Vec<PathBuf>,
        branches: Vec<String>,
    },
    /// `giverny-manage` run as another session.
    SessionAs(String),
}

/// A `pgrep`-style pattern.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pattern {
    pub text: String,
    /// Matched against the whole command line (`-f`), not the name.
    pub full: bool,
    /// The whole name (`-x`, `killall`'s default).
    pub exact: bool,
    pub icase: bool,
}

/// Where the shell is as the text is read: `cd` moves it.
struct State {
    cwd: PathBuf,
    /// A `kill` with no pid of its own: fed by `$(pgrep …)` or `xargs`.
    bare_kill: bool,
    /// What `pgrep`/`pidof` in the same text match.
    lists: Vec<Pattern>,
    /// `CLAUDE_CODE_SESSION_ID=<id>` set in the text.
    session_as: Option<String>,
}

/// What `cmd`, run in `cwd`, reaches for.
pub fn reaches(cmd: &str, cwd: &Path) -> Vec<Reach> {
    let mut st = State {
        cwd: cwd.to_path_buf(),
        bare_kill: false,
        lists: Vec::new(),
        session_as: None,
    };
    let mut out = Vec::new();
    text(cmd, 0, &mut st, &mut out);
    if st.bare_kill {
        out.extend(st.lists.drain(..).map(Reach::Match));
    }
    out
}

fn text(cmd: &str, depth: usize, st: &mut State, out: &mut Vec<Reach>) {
    if depth > MAX_DEPTH {
        return;
    }
    let parsed = split(cmd);
    for nested in &parsed.nested {
        text(nested, depth + 1, st, out);
    }
    for seg in &parsed.segments {
        words(&seg.words, depth, st, out);
        // A heredoc's lines are read as commands, as before the split kept
        // them apart.
        if let Some(body) = &seg.heredoc {
            text(body, depth + 1, st, out);
        }
    }
}

fn session_var(word: &str) -> Option<String> {
    word.strip_prefix(SESSION_ENV)?
        .strip_prefix('=')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn words(ws: &[String], depth: usize, st: &mut State, out: &mut Vec<Reach>) {
    let mut i = 0;
    while i < ws.len() && is_assignment(&ws[i]) {
        if let Some(s) = session_var(&ws[i]) {
            st.session_as = Some(s);
        }
        i += 1;
    }
    let ws = &ws[i..];
    let Some(first) = ws.first() else {
        return;
    };
    let prog = base(first);
    let args = &ws[1..];
    let from = |j: usize| args.get(j..).unwrap_or_default();
    if LEADERS.contains(&prog) {
        return words(args, depth, st, out);
    }
    match prog {
        "sudo" | "doas" => {
            let j = after_options(args, &["-u", "-g", "-C", "-p", "-D"]);
            words(from(j), depth, st, out);
        }
        "env" => {
            let mut j = 0;
            while j < args.len() {
                let a = args[j].as_str();
                if a == "-S" || a == "--split-string" {
                    let inner = args.get(j + 1).cloned().unwrap_or_default();
                    let tail = from(j + 2).join(" ");
                    return text(&format!("{inner} {tail}"), depth + 1, st, out);
                }
                if let Some(s) = session_var(a) {
                    st.session_as = Some(s);
                }
                if matches!(a, "-u" | "--unset" | "-C" | "--chdir") {
                    j += 2;
                } else if a.starts_with('-') || is_assignment(a) {
                    j += 1;
                } else {
                    break;
                }
            }
            words(from(j), depth, st, out);
        }
        "export" => {
            for a in args {
                if let Some(s) = session_var(a) {
                    st.session_as = Some(s);
                }
            }
        }
        "timeout" => {
            let j = after_options(args, &["-s", "--signal", "-k", "--kill-after"]);
            words(from(j + 1), depth, st, out);
        }
        "xargs" => {
            let j = after_options(args, &["-I", "-n", "-P", "-L", "-s", "-d", "-E", "-a"]);
            words(from(j), depth, st, out);
        }
        "stdbuf" | "watch" => {
            let j = after_options(args, &["-n", "-i", "-o", "-e"]);
            words(from(j), depth, st, out);
        }
        "nice" => words(from(after_nice(args)), depth, st, out),
        "ionice" => {
            let j = after_options(args, &["-c", "-n", "--class", "--classdata"]);
            words(from(j), depth, st, out);
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
            let mut j = 0;
            let mut script = false;
            while j < args.len() && args[j].starts_with('-') && args[j] != "-" {
                if !args[j].starts_with("--") && args[j][1..].contains('c') {
                    script = true;
                }
                j += 1;
            }
            if script {
                text(args.get(j).map_or("", String::as_str), depth + 1, st, out);
            }
        }
        "eval" => text(&args.join(" "), depth + 1, st, out),
        "giverny-manage" => manage(args, depth, st, out),
        "giverny" if args.first().is_some_and(|a| a == "manage") => {
            manage(&args[1..], depth, st, out);
        }
        "cd" | "pushd" => {
            let to = args.iter().find(|a| !a.starts_with('-'));
            st.cwd = match to.map(String::as_str) {
                None | Some("~") => dirs::home_dir().unwrap_or_else(|| st.cwd.clone()),
                Some(d) => under(&st.cwd, d),
            };
        }
        "kill" => match kill_targets(args) {
            None => {}
            Some(t) if t.is_empty() => st.bare_kill = true,
            Some(t) => out.push(Reach::Pids(t)),
        },
        "pkill" => out.extend(pgrep_pattern(args, true).map(Reach::Match)),
        "pgrep" => st.lists.extend(pgrep_pattern(args, false)),
        "killall" => out.extend(killall_names(args).into_iter().map(Reach::Match)),
        "pidof" => st.lists.extend(
            args.iter()
                .filter(|a| !a.starts_with('-'))
                .map(|n| Pattern {
                    text: n.clone(),
                    exact: true,
                    ..Pattern::default()
                }),
        ),
        "systemctl" => out.extend(units(args).map(Reach::Units)),
        "git" => out.extend(git(args, &st.cwd)),
        _ => {}
    }
}

/// `d` read from `cwd`, `~` expanded.
fn under(cwd: &Path, d: &str) -> PathBuf {
    if let Some(rest) = d.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    cwd.join(d)
}

/// `giverny manage …`: a `run`'s command is read on; any other subcommand
/// under another session's id is a reach.
fn manage(args: &[String], depth: usize, st: &mut State, out: &mut Vec<Reach>) {
    if args.first().is_some_and(|a| a == "run")
        && let Some(dash) = args.iter().position(|a| a == "--")
    {
        return words(&args[dash + 1..], depth, st, out);
    }
    if let Some(s) = st.session_as.clone() {
        out.push(Reach::SessionAs(s));
    }
}

/// `kill`'s pids; `None` for a listing (`-l`). A word that is no number
/// (`$pid`, `%1`) is not resolvable and left out.
fn kill_targets(args: &[String]) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    let mut opts = true;
    let mut signal = false;
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        if opts {
            if a == "--" {
                opts = false;
                j += 1;
                continue;
            }
            if matches!(a, "-l" | "-L" | "--list" | "--table") {
                return None;
            }
            if matches!(a, "-s" | "-n" | "--signal") {
                signal = true;
                j += 2;
                continue;
            }
            if a.len() > 1 && a.starts_with('-') && !signal {
                signal = true; // `-9`, `-KILL`, `-SIGTERM`
                j += 1;
                continue;
            }
        }
        if let Ok(n) = a.parse::<i64>() {
            out.push(n);
        }
        j += 1;
    }
    Some(out)
}

/// `pkill`'s (`kill`) or `pgrep`'s pattern; `None` when an option narrows
/// the match in a way not followed here (`-P`, `-g`, `-n`, `-v`, …): not
/// resolvable, so allowed.
fn pgrep_pattern(args: &[String], kill: bool) -> Option<Pattern> {
    let mut p = Pattern::default();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        if a == "--" {
            p.text = args.get(j + 1)?.clone();
            return Some(p);
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, _)) => (n, true),
                None => (long, false),
            };
            match name {
                "full" => p.full = true,
                "exact" => p.exact = true,
                "ignore-case" => p.icase = true,
                "signal" | "euid" | "uid" | "group" | "delimiter" | "queue" if !inline => j += 1,
                "inverse" | "parent" | "pgroup" | "session" | "terminal" | "pidfile" | "ns"
                | "nslist" | "newest" | "oldest" | "older" | "runstates" | "cgroup" | "env" => {
                    return None;
                }
                _ => {}
            }
            j += 1;
            continue;
        }
        if let Some(short) = a.strip_prefix('-')
            && !short.is_empty()
        {
            let signal = short.chars().all(|c| c.is_ascii_digit())
                || short.chars().all(|c| c.is_ascii_uppercase());
            if kill && signal {
                j += 1;
                continue;
            }
            for (k, c) in short.char_indices() {
                match c {
                    'f' => p.full = true,
                    'x' => p.exact = true,
                    'i' => p.icase = true,
                    'v' | 'n' | 'o' | 'P' | 'g' | 's' | 't' | 'F' | 'O' | 'r' => return None,
                    'u' | 'U' | 'G' | 'd' | 'q' => {
                        if k + 1 == short.len() {
                            j += 1; // its value is the next word
                        }
                        break;
                    }
                    _ => {}
                }
            }
            j += 1;
            continue;
        }
        p.text = a.to_string();
        return Some(p);
    }
    None
}

/// `killall`'s names, each matched whole unless `-r`.
fn killall_names(args: &[String]) -> Vec<Pattern> {
    let (mut exact, mut icase) = (true, false);
    let mut names = Vec::new();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        match a {
            "-s" | "--signal" | "-u" | "--user" => j += 1,
            "-r" | "--regexp" => exact = false,
            "-I" | "--ignore-case" => icase = true,
            "-o" | "-y" | "--older-than" | "--younger-than" | "-n" | "--ns" | "-Z"
            | "--context" => return Vec::new(),
            _ if a.starts_with('-') && a.len() > 1 => {}
            _ => names.push(a.to_string()),
        }
        j += 1;
    }
    names
        .into_iter()
        .map(|text| Pattern {
            text,
            full: false,
            exact,
            icase,
        })
        .collect()
}

/// `systemctl stop|kill|…`'s units.
fn units(args: &[String]) -> Option<Vec<String>> {
    const STOPS: &[&str] = &[
        "stop",
        "kill",
        "restart",
        "try-restart",
        "reload-or-restart",
        "try-reload-or-restart",
        "freeze",
        "clean",
    ];
    const WITH_VALUE: &[&str] = &[
        "-s",
        "--signal",
        "--kill-whom",
        "--kill-value",
        "-p",
        "--property",
        "-H",
        "--host",
        "-M",
        "--machine",
        "--what",
        "--job-mode",
    ];
    let mut words = Vec::new();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        if WITH_VALUE.contains(&a) {
            j += 2;
            continue;
        }
        if !a.starts_with('-') {
            words.push(a.to_string());
        }
        j += 1;
    }
    let (verb, units) = words.split_first()?;
    (STOPS.contains(&verb.as_str()) && !units.is_empty()).then(|| units.to_vec())
}

/// git's mutating verbs: what they change is the worktree they run in.
const GIT_MUTATES: &[&str] = &[
    "checkout",
    "switch",
    "reset",
    "restore",
    "clean",
    "commit",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "am",
    "pull",
    "rm",
    "mv",
    "add",
    "apply",
];

fn git(args: &[String], cwd: &Path) -> Option<Reach> {
    let mut dir = cwd.to_path_buf();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        match a {
            "-C" => {
                dir = under(&dir, args.get(j + 1)?);
                j += 2;
            }
            "--work-tree" => {
                dir = under(&dir, args.get(j + 1)?);
                j += 2;
            }
            "-c" | "--git-dir" | "--namespace" | "--exec-path" | "--config-env" => j += 2,
            _ if a.starts_with("--work-tree=") => {
                dir = under(&dir, &a["--work-tree=".len()..]);
                j += 1;
            }
            _ if a.starts_with('-') => j += 1,
            _ => break,
        }
    }
    let verb = args.get(j)?.as_str();
    let rest: Vec<&String> = args.get(j + 1..).unwrap_or_default().iter().collect();
    let plain: Vec<&str> = rest
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|a| a.as_str())
        .collect();
    let reach = |what: String, dir: Option<PathBuf>, paths: Vec<PathBuf>, branches: Vec<String>| {
        Some(Reach::Git {
            what,
            dir,
            paths,
            branches,
        })
    };
    match verb {
        "stash" if plain.first().is_none_or(|s| *s != "list" && *s != "show") => {
            reach("`git stash`".into(), Some(dir), vec![], vec![])
        }
        "stash" => None,
        "worktree" => match plain.first().copied() {
            Some(sub @ ("remove" | "move")) => {
                let path = plain.get(1).map(|p| under(&dir, p));
                reach(
                    format!("`git worktree {sub}`"),
                    None,
                    path.into_iter().collect(),
                    vec![],
                )
            }
            _ => None,
        },
        "branch" => {
            let deletes = rest.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "-D" | "--delete" | "-f" | "--force" | "-m" | "-M" | "--move"
                )
            });
            (deletes && !plain.is_empty()).then(|| Reach::Git {
                what: "`git branch` deleting or moving a branch".into(),
                dir: None,
                paths: vec![],
                branches: plain.iter().map(|b| b.to_string()).collect(),
            })
        }
        v if GIT_MUTATES.contains(&v) => reach(format!("`git {v}`"), Some(dir), vec![], vec![]),
        _ => None,
    }
}

/// The linked worktree `path` is in (a directory whose `.git` is a file),
/// or `None` in the main checkout — everyone's — or outside any repo.
pub fn linked_root(path: &Path) -> Option<PathBuf> {
    let mut p = Some(path);
    while let Some(d) = p {
        let git = d.join(".git");
        if git.is_file() {
            return Some(d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));
        }
        if git.is_dir() {
            return None;
        }
        p = d.parent();
    }
    None
}

/// The branch a linked worktree has checked out, from its `.git` file.
fn head_branch(root: &Path) -> Option<String> {
    let git = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = git.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = root.join(gitdir);
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// Who a process, a run scope or a worktree belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Owner {
    pub session: Option<String>,
    /// The claude process, when it is a claude's tree.
    pub claude: Option<u32>,
    /// The task, when it is a run's.
    pub task: Option<String>,
}

/// The caller: its session, and its claude process when found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    pub session: String,
    pub claude: Option<u32>,
}

impl Owner {
    /// Is it someone else's: another session, or another live claude.
    pub fn foreign(&self, me: &Me) -> bool {
        if self.claude.is_some() && self.claude == me.claude {
            return false;
        }
        match &self.session {
            Some(s) => *s != me.session,
            None => self.claude.is_some() && me.claude.is_some(),
        }
    }
}

/// One `giverny manage run` command's record.
pub use crate::run_live::RunRecord;

/// What the guard reads of the machine; [`System`] is the real one.
pub trait World {
    /// `/proc/<pid>/cgroup`.
    fn cgroup(&self, pid: u32) -> Option<String>;
    fn parent(&self, pid: u32) -> Option<u32>;
    fn is_claude(&self, pid: u32) -> bool;
    fn alive(&self, pid: u32) -> bool;
    /// The session a claude process runs, from Claude Code's registry.
    fn session_of(&self, claude: u32) -> Option<String>;
    /// Every run's record, its `run` alive or not.
    fn runs(&self) -> Vec<RunRecord>;
    /// Where a live run works now (else where it started).
    fn run_cwd(&self, run: &RunRecord) -> Option<PathBuf> {
        run.cwd.clone()
    }
    /// Every unexpired lease: (session, task).
    fn leases(&self) -> Vec<(String, String)>;
    /// Every process: (pid, name, command line with spaces).
    fn procs(&self) -> Vec<(u32, String, String)>;
    /// The scope units in Giverny's slice (`giverny-run-x-1.scope`, …).
    fn scopes(&self) -> Vec<String>;
    /// Every live claude: (pid, session, cwd).
    fn sessions(&self) -> Vec<(u32, String, PathBuf)>;
    /// A session's recorded name.
    fn name(&self, session: &str) -> Option<String>;
}

/// The name part of a run's unit, as [`crate::manage_run::unit_name`]
/// makes it from the task.
fn unit_task(task: &str) -> String {
    let unit = crate::manage_run::unit_name(task, 0);
    unit["giverny-run-".len()..unit.len() - 2].to_string()
}

/// A Giverny unit's owner: `Some(None)` for one of Giverny's units nobody
/// live owns (its claude gone, its run gone with no lease left), `None`
/// for a unit that is not Giverny's.
pub fn owner_of_unit(w: &dyn World, unit: &str) -> Option<Option<Owner>> {
    let u = unit
        .strip_suffix(".scope")
        .or_else(|| unit.strip_suffix(".service"))
        .unwrap_or(unit);
    if let Some(rest) = u.strip_prefix("giverny-run-") {
        let (t, pid) = rest.rsplit_once('-')?;
        let pid: u32 = pid.parse().ok()?;
        let leases = w.leases();
        let known = w
            .runs()
            .into_iter()
            .find(|r| r.pid == pid)
            .map(|r| (r.session, r.task));
        let (session, task) = match known {
            Some(x) => x,
            None => {
                let holders: Vec<&(String, String)> = leases
                    .iter()
                    .filter(|(_, task)| unit_task(task) == t)
                    .collect();
                let sessions: HashSet<&String> = holders.iter().map(|(s, _)| s).collect();
                if sessions.len() != 1 {
                    return Some(None);
                }
                holders[0].clone()
            }
        };
        let live = w.alive(pid) || leases.iter().any(|(s, k)| *s == session && *k == task);
        return Some(live.then_some(Owner {
            session: Some(session),
            claude: None,
            task: Some(task),
        }));
    }
    if let Some(rest) = u.strip_prefix("giverny-claude-") {
        let pid: u32 = rest.parse().ok()?;
        return Some(w.alive(pid).then(|| Owner {
            session: w.session_of(pid),
            claude: Some(pid),
            task: None,
        }));
    }
    None
}

/// A process's owner: the innermost Giverny scope in its cgroup, else the
/// claude among its parents (the caller's own, if it is one of them).
pub fn owner_of_pid(w: &dyn World, me: &Me, pid: u32) -> Option<Owner> {
    if let Some(cg) = w.cgroup(pid) {
        let path = cg.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("");
        for part in path.rsplit('/') {
            if let Some(o) = owner_of_unit(w, part) {
                return o;
            }
        }
    }
    let mut p = pid;
    let mut first = None;
    for _ in 0..64 {
        if w.is_claude(p) {
            if Some(p) == me.claude {
                return Some(Owner {
                    session: Some(me.session.clone()),
                    claude: Some(p),
                    task: None,
                });
            }
            first.get_or_insert(p);
        }
        match w.parent(p) {
            Some(q) if q != p && q != 0 => p = q,
            _ => break,
        }
    }
    first.map(|c| Owner {
        session: w.session_of(c),
        claude: Some(c),
        task: None,
    })
}

/// A glob as systemctl reads one: `*`, `?`; `None` for one with `[`.
fn glob(pat: &str, s: &str) -> Option<bool> {
    if pat.contains('[') {
        return None;
    }
    let toks: Vec<Tok> = pat
        .chars()
        .map(|c| match c {
            '*' => Tok::Run,
            '?' => Tok::Any,
            c => Tok::Lit(c),
        })
        .collect();
    Some(tok_match(&toks, &s.chars().collect::<Vec<_>>()))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Lit(char),
    Any,
    Run,
}

fn tok_match(p: &[Tok], s: &[char]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((Tok::Run, rest)) => (0..=s.len()).any(|k| tok_match(rest, &s[k..])),
        Some((Tok::Any, rest)) => !s.is_empty() && tok_match(rest, &s[1..]),
        Some((Tok::Lit(c), rest)) => s.first() == Some(c) && tok_match(rest, &s[1..]),
    }
}

/// A `pgrep` pattern as tokens, unanchored ends as runs; `None` for regex
/// syntax beyond `.`, `.*`, `^`, `$` and `\x`.
fn regex_toks(p: &Pattern) -> Option<Vec<Tok>> {
    let mut text: Vec<char> = p.text.chars().collect();
    if p.icase {
        text = text.iter().flat_map(|c| c.to_lowercase()).collect();
    }
    let mut start = p.exact;
    let mut end = p.exact;
    if text.first() == Some(&'^') {
        start = true;
        text.remove(0);
    }
    if text.last() == Some(&'$') && !text.ends_with(&['\\', '$']) {
        end = true;
        text.pop();
    }
    let mut toks = Vec::new();
    if !start {
        toks.push(Tok::Run);
    }
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            '\\' => {
                toks.push(Tok::Lit(*text.get(i + 1)?));
                i += 1;
            }
            '.' if text.get(i + 1) == Some(&'*') => {
                toks.push(Tok::Run);
                i += 1;
            }
            '.' => toks.push(Tok::Any),
            '[' | ']' | '(' | ')' | '|' | '+' | '?' | '*' | '{' | '}' | '^' | '$' => return None,
            c => toks.push(Tok::Lit(c)),
        }
        i += 1;
    }
    if !end {
        toks.push(Tok::Run);
    }
    Some(toks)
}

/// The processes `p` matches, the way `pgrep`/`killall` would.
fn matching(w: &dyn World, p: &Pattern) -> Vec<u32> {
    let Some(toks) = regex_toks(p) else {
        return Vec::new();
    };
    let fold = |s: &str| -> Vec<char> {
        if p.icase {
            s.chars().flat_map(|c| c.to_lowercase()).collect()
        } else {
            s.chars().collect()
        }
    };
    w.procs()
        .into_iter()
        .filter(|(_, comm, cmdline)| {
            if p.full {
                return tok_match(&toks, &fold(cmdline));
            }
            let argv0 = cmdline.split(' ').next().map(base).unwrap_or_default();
            tok_match(&toks, &fold(comm)) || (p.exact && tok_match(&toks, &fold(argv0)))
        })
        .map(|(pid, _, _)| pid)
        .collect()
}

/// Every linked worktree another session's live worker works in, and its
/// owner.
pub fn held_worktrees(w: &dyn World, me: &Me) -> Vec<(PathBuf, Owner)> {
    let mut out: Vec<(PathBuf, Owner)> = Vec::new();
    for r in w.runs() {
        if r.session == me.session || !w.alive(r.pid) {
            continue;
        }
        if let Some(root) = w.run_cwd(&r).as_deref().and_then(linked_root) {
            out.push((
                root,
                Owner {
                    session: Some(r.session.clone()),
                    claude: None,
                    task: Some(r.task.clone()),
                },
            ));
        }
    }
    for (pid, session, cwd) in w.sessions() {
        if session == me.session || Some(pid) == me.claude || cwd.as_os_str().is_empty() {
            continue;
        }
        if let Some(root) = linked_root(&cwd)
            && !out.iter().any(|(r, _)| *r == root)
        {
            out.push((
                root,
                Owner {
                    session: Some(session),
                    claude: Some(pid),
                    task: None,
                },
            ));
        }
    }
    out
}

/// Whose worktree `path` is in, when another session's.
fn worktree_owner(w: &dyn World, me: &Me, path: &Path) -> Option<Owner> {
    let root = linked_root(path)?;
    held_worktrees(w, me)
        .into_iter()
        .find(|(r, _)| *r == root)
        .map(|(_, o)| o)
}

/// Why `cmd` (run in `cwd` by `me`) is refused, or `None`.
pub fn verdict(cmd: &str, cwd: &Path, me: &Me, w: &dyn World) -> Option<String> {
    for reach in reaches(cmd, cwd) {
        if let Some(why) = judge(&reach, me, w) {
            return Some(why);
        }
    }
    None
}

/// Why an Edit/Write of `path` by `me` is refused, or `None`.
pub fn edit_verdict(path: &Path, me: &Me, w: &dyn World) -> Option<String> {
    let dir = path.parent().unwrap_or(path);
    let o = worktree_owner(w, me, dir)?;
    let root = linked_root(dir)?;
    Some(refusal(
        &format!("editing a file in `{}`", root.display()),
        &o,
        "works in that worktree",
        w,
    ))
}

/// Why `reach` (by `me`) is refused, or `None`.
pub fn judge(reach: &Reach, me: &Me, w: &dyn World) -> Option<String> {
    match reach {
        Reach::Pids(pids) => {
            let targets: Vec<u32> = if pids.contains(&-1) {
                w.procs().into_iter().map(|(p, _, _)| p).collect()
            } else {
                pids.iter()
                    .filter(|p| **p != 0)
                    .map(|p| p.unsigned_abs() as u32)
                    .collect()
            };
            first_foreign(w, me, &targets)
                .map(|(pid, o)| refusal(&format!("killing pid {pid}"), &o, "owns it", w))
        }
        Reach::Match(p) => {
            let pids = matching(w, p);
            first_foreign(w, me, &pids).map(|(pid, o)| {
                refusal(
                    &format!("killing what `{}` matches (pid {pid} among them)", p.text),
                    &o,
                    "owns it",
                    w,
                )
            })
        }
        Reach::Units(units) => {
            for u in units {
                let concrete: Vec<String> =
                    if matches!(u.as_str(), "giverny-claude.slice" | "giverny.slice") {
                        w.scopes()
                    } else if u.contains(['*', '?', '[']) {
                        w.scopes()
                            .into_iter()
                            .filter(|s| {
                                glob(u, s).unwrap_or(false)
                                    || glob(&format!("{u}.scope"), s).unwrap_or(false)
                            })
                            .collect()
                    } else {
                        vec![u.clone()]
                    };
                for c in concrete {
                    if let Some(Some(o)) = owner_of_unit(w, &c)
                        && o.foreign(me)
                    {
                        return Some(refusal(&format!("stopping `{c}`"), &o, "owns it", w));
                    }
                }
            }
            None
        }
        Reach::Git {
            what,
            dir,
            paths,
            branches,
        } => {
            for p in dir.iter().chain(paths) {
                if let Some(o) = worktree_owner(w, me, p) {
                    let root = linked_root(p).unwrap_or_else(|| p.clone());
                    return Some(refusal(
                        &format!("{what} in `{}`", root.display()),
                        &o,
                        "works in that worktree",
                        w,
                    ));
                }
            }
            if !branches.is_empty() {
                for (root, o) in held_worktrees(w, me) {
                    if let Some(b) = head_branch(&root)
                        && branches.contains(&b)
                    {
                        return Some(refusal(
                            &format!("{what} `{b}`, checked out in `{}`", root.display()),
                            &o,
                            "works in that worktree",
                            w,
                        ));
                    }
                }
            }
            None
        }
        Reach::SessionAs(s) => speak_as(s, me, w),
    }
}

/// Why `giverny-manage`, run by `me` as `session` (`--session`,
/// `CLAUDE_CODE_SESSION_ID=`), is refused: that session runs in another
/// live claude. A session no claude runs now (one this claude was before
/// a `/clear`, one long gone) is anyone's to tidy.
pub fn speak_as(session: &str, me: &Me, w: &dyn World) -> Option<String> {
    if session == me.session {
        return None;
    }
    let o = foreign_live_session(w, me, session)?;
    Some(refusal(
        &format!("running `giverny-manage` as session {}", short(session)),
        &o,
        "is that session",
        w,
    ))
}

/// The first of `pids` another session owns.
fn first_foreign(w: &dyn World, me: &Me, pids: &[u32]) -> Option<(u32, Owner)> {
    pids.iter().find_map(|&pid| {
        let o = owner_of_pid(w, me, pid)?;
        o.foreign(me).then_some((pid, o))
    })
}

/// `session` when it runs in a live claude other than the caller's.
pub fn foreign_live_session(w: &dyn World, me: &Me, session: &str) -> Option<Owner> {
    w.sessions()
        .into_iter()
        .find(|(pid, s, _)| s == session && Some(*pid) != me.claude)
        .map(|(pid, s, _)| Owner {
            session: Some(s),
            claude: Some(pid),
            task: None,
        })
}

fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

/// The message a refused reach is answered with: what, whose, and the
/// way to ask.
pub fn refusal(what: &str, o: &Owner, verb: &str, w: &dyn World) -> String {
    let mut who = match (&o.session, o.claude) {
        (Some(s), _) => match w.name(s) {
            Some(n) => format!("session {} ({n})", short(s)),
            None => format!("session {}", short(s)),
        },
        (None, Some(pid)) => format!("the claude at pid {pid}"),
        (None, None) => "another session".to_string(),
    };
    if let Some(t) = &o.task {
        who = format!("{who}, for its task {t},");
    }
    let target = o
        .task
        .clone()
        .or_else(|| o.session.clone())
        .unwrap_or_else(|| "<task>".into());
    format!(
        "Giverny refused this: {what} — {who} {verb}. One manager session never \
         reaches into another's processes, run scopes, ledger rows or worktrees. \
         Ask its owner instead: `giverny-manage ask {target} \"<what you need and why>\"` \
         (it reaches that session on its next tool call), or ask the user."
    )
}

/// The machine as it is: `/proc`, Giverny's feeds and Claude Code's
/// registry. Read lazily: only what a reach asks for.
pub struct System {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    feeds: PathBuf,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    config_dirs: std::cell::OnceCell<Vec<PathBuf>>,
}

impl System {
    pub fn new(feeds: PathBuf) -> System {
        System {
            feeds,
            config_dirs: std::cell::OnceCell::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn config_dirs(&self) -> &[PathBuf] {
        self.config_dirs
            .get_or_init(|| crate::registry::ambient_config_dirs(None))
    }

    #[cfg(target_os = "linux")]
    fn ledger(&self) -> PathBuf {
        crate::resources::ledger_path(&self.feeds)
    }

    /// The caller of a hook or a `giverny-manage` command: `session`, and
    /// the first claude above this process.
    pub fn me(&self, session: &str) -> Me {
        let mut claude = None;
        let mut p = std::process::id();
        for _ in 0..64 {
            if p != std::process::id() && self.is_claude(p) {
                claude = Some(p);
                break;
            }
            match self.parent(p) {
                Some(q) if q != p && q != 0 => p = q,
                _ => break,
            }
        }
        Me {
            session: session.to_string(),
            claude,
        }
    }
}

#[cfg(target_os = "linux")]
impl World for System {
    fn cgroup(&self, pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()
    }
    fn parent(&self, pid: u32) -> Option<u32> {
        crate::lineage::proc_parent(pid)
    }
    fn is_claude(&self, pid: u32) -> bool {
        crate::lineage::proc_is_claude(pid)
    }
    fn alive(&self, pid: u32) -> bool {
        crate::run_live::alive(pid)
    }
    fn session_of(&self, claude: u32) -> Option<String> {
        self.config_dirs().iter().find_map(|d| {
            let bytes = std::fs::read(d.join("sessions").join(format!("{claude}.json"))).ok()?;
            let e: crate::registry::SessionEntry = serde_json::from_slice(&bytes).ok()?;
            // Its start time too: a file left by a claude that died in a
            // reboot names a pid that may be this one again.
            (e.pid == claude && crate::registry::entry_is_live(d, &e)).then_some(e.session_id)
        })
    }
    fn runs(&self) -> Vec<RunRecord> {
        crate::run_live::records(&crate::run_live::runs_dir(&self.ledger()))
    }
    fn run_cwd(&self, run: &RunRecord) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", run.pid))
            .ok()
            .or_else(|| run.cwd.clone())
    }
    fn leases(&self) -> Vec<(String, String)> {
        crate::manage_inbox::read_ledger(&self.ledger(), crate::manage::now_ms())
            .map(|l| l.leases.into_iter().map(|l| (l.session, l.task)).collect())
            .unwrap_or_default()
    }
    fn procs(&self) -> Vec<(u32, String, String)> {
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| *pid != std::process::id())
            .filter_map(|pid| {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
                let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let cmd: Vec<String> = cmd
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();
                Some((pid, comm.trim().to_string(), cmd.join(" ")))
            })
            .collect()
    }
    fn scopes(&self) -> Vec<String> {
        let slice = crate::tab_cap::SLICE;
        let mine = self.cgroup(std::process::id()).unwrap_or_default();
        let path = mine
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .unwrap_or("");
        let dir = match path.find(slice) {
            Some(at) => format!("/sys/fs/cgroup{}", &path[..at + slice.len()]),
            None => {
                // SAFETY: getuid cannot fail.
                let uid = unsafe { libc::getuid() };
                format!(
                    "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/giverny.slice/{slice}"
                )
            }
        };
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .filter(|n| n.ends_with(".scope"))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn sessions(&self) -> Vec<(u32, String, PathBuf)> {
        crate::registry::scan(self.config_dirs().to_vec())
            .into_iter()
            .map(|s| (s.entry.pid, s.entry.session_id, s.entry.cwd))
            .collect()
    }
    fn name(&self, session: &str) -> Option<String> {
        crate::names::read(&self.feeds, session)
    }
}

/// Nothing is read off Linux: every reach resolves to no owner.
#[cfg(not(target_os = "linux"))]
impl World for System {
    fn cgroup(&self, _: u32) -> Option<String> {
        None
    }
    fn parent(&self, _: u32) -> Option<u32> {
        None
    }
    fn is_claude(&self, _: u32) -> bool {
        false
    }
    fn alive(&self, _: u32) -> bool {
        false
    }
    fn session_of(&self, _: u32) -> Option<String> {
        None
    }
    fn runs(&self) -> Vec<RunRecord> {
        Vec::new()
    }
    fn leases(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn procs(&self) -> Vec<(u32, String, String)> {
        Vec::new()
    }
    fn scopes(&self) -> Vec<String> {
        Vec::new()
    }
    fn sessions(&self) -> Vec<(u32, String, PathBuf)> {
        Vec::new()
    }
    fn name(&self, _: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Two sessions: mine (`me`, claude 100) and Rotem's (`other`, claude
    /// 200), each with a run, and a process of nobody's.
    #[derive(Default)]
    struct Fake {
        cgroups: HashMap<u32, String>,
        parents: HashMap<u32, u32>,
        claudes: HashMap<u32, String>,
        alive: HashSet<u32>,
        runs: Vec<RunRecord>,
        leases: Vec<(String, String)>,
        procs: Vec<(u32, String, String)>,
        scopes: Vec<String>,
        cwds: HashMap<u32, PathBuf>,
    }

    impl World for Fake {
        fn cgroup(&self, pid: u32) -> Option<String> {
            self.cgroups.get(&pid).cloned()
        }
        fn parent(&self, pid: u32) -> Option<u32> {
            self.parents.get(&pid).copied()
        }
        fn is_claude(&self, pid: u32) -> bool {
            self.claudes.contains_key(&pid)
        }
        fn alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
        fn session_of(&self, claude: u32) -> Option<String> {
            self.claudes.get(&claude).cloned()
        }
        fn runs(&self) -> Vec<RunRecord> {
            self.runs.clone()
        }
        fn leases(&self) -> Vec<(String, String)> {
            self.leases.clone()
        }
        fn procs(&self) -> Vec<(u32, String, String)> {
            self.procs.clone()
        }
        fn scopes(&self) -> Vec<String> {
            self.scopes.clone()
        }
        fn sessions(&self) -> Vec<(u32, String, PathBuf)> {
            self.claudes
                .iter()
                .filter(|(p, _)| self.alive.contains(p))
                .map(|(p, s)| (*p, s.clone(), self.cwds.get(p).cloned().unwrap_or_default()))
                .collect()
        }
        fn name(&self, session: &str) -> Option<String> {
            (session == "other-session").then(|| "Rotem Ashby".to_string())
        }
    }

    const SLICE_PATH: &str =
        "0::/user.slice/user-1000.slice/user@1000.service/giverny.slice/giverny-claude.slice";

    fn me() -> Me {
        Me {
            session: "my-session".into(),
            claude: Some(100),
        }
    }

    fn world() -> Fake {
        let mut f = Fake::default();
        f.claudes.insert(100, "my-session".into());
        f.claudes.insert(200, "other-session".into());
        f.alive
            .extend([100, 200, 110, 210, 300, 400, 500, 600, 700]);
        // My claude's commands, in its tab scope.
        f.cgroups
            .insert(110, format!("{SLICE_PATH}/giverny-claude-100.scope\n"));
        f.parents.insert(110, 100);
        // Rotem's: a test worker orphaned (no parent), still in its scope.
        f.cgroups
            .insert(210, format!("{SLICE_PATH}/giverny-claude-200.scope\n"));
        // Rotem's run of scale343 (run pid 300) and its vitest (400).
        f.cgroups.insert(
            400,
            format!("{SLICE_PATH}/giverny-run-scale343-300.scope\n"),
        );
        // My run of g415 (run pid 500) and its cargo (600).
        f.cgroups
            .insert(600, format!("{SLICE_PATH}/giverny-run-g415-500.scope\n"));
        // Nobody's: the person's own shell.
        f.cgroups.insert(
            700,
            "0::/user.slice/user-1000.slice/session-2.scope\n".into(),
        );
        f.runs = vec![
            RunRecord {
                pid: 300,
                session: "other-session".into(),
                task: "scale343".into(),
                cwd: None,
            },
            RunRecord {
                pid: 500,
                session: "my-session".into(),
                task: "g415".into(),
                cwd: None,
            },
        ];
        f.leases = vec![
            ("other-session".into(), "scale343".into()),
            ("my-session".into(), "g415".into()),
        ];
        f.procs = vec![
            (110, "node".into(), "node vitest my-tests".into()),
            (
                210,
                "node".into(),
                "node /x/node_modules/vitest/worker.js".into(),
            ),
            (400, "node".into(), "node vitest run".into()),
            (600, "cargo".into(), "cargo test -p giverny-claude".into()),
            (700, "bash".into(), "bash".into()),
        ];
        f.scopes = vec![
            "giverny-claude-100.scope".into(),
            "giverny-claude-200.scope".into(),
            "giverny-run-scale343-300.scope".into(),
            "giverny-run-g415-500.scope".into(),
        ];
        f
    }

    fn says(cmd: &str, w: &Fake) -> Option<String> {
        verdict(cmd, Path::new("/home/user/giverny"), &me(), w)
    }

    #[test]
    fn killing_another_sessions_process_is_refused_with_the_owner_and_the_ask() {
        let w = world();
        for cmd in [
            "kill 400",
            "kill -9 400",
            "kill -KILL 210",
            "kill -s TERM 400",
            "kill -- -400",
            "sleep 1; kill -9 210 2>/dev/null || true",
            "bash -c 'kill 400'",
            "pkill -f vitest/worker",
            "pkill -9 -f 'vitest run'",
            "kill $(pgrep -f 'vitest run')",
            "pgrep -f worker.js | xargs kill -9",
            "kill -9 -1",
        ] {
            let r = says(cmd, &w).unwrap_or_else(|| panic!("{cmd}: not refused"));
            assert!(r.contains("session other-se (Rotem Ashby)"), "{cmd}: {r}");
            assert!(r.contains("giverny-manage ask"), "{cmd}: {r}");
        }
        let r = says("kill 400", &w).unwrap();
        assert!(r.contains("for its task scale343"), "{r}");
        assert!(r.contains("`giverny-manage ask scale343 "), "{r}");
        assert!(r.contains("killing pid 400"), "{r}");
        let r = says("kill 210", &w).unwrap();
        assert!(r.contains("`giverny-manage ask other-session "), "{r}");
    }
}
