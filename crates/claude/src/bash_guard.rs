//! The plugin's `PreToolUse` Bash guard: a command that would take work
//! out of its tab's resource cap is let through with a reminder, not
//! refused.
//!
//! Every claude in a Giverny tab runs in a capped scope ([`crate::tab_cap`])
//! and everything it starts inherits that cap — unless the command asks
//! systemd or the kernel to put it somewhere else. `systemd-run --user`
//! makes a scope of its own outside every cap; `systemctl --user
//! set-property` lifts a cap; a write into `/sys/fs/cgroup` moves a process
//! or changes a limit (the user owns their manager's cgroup tree);
//! `cgexec`, `taskset`, `chrt`, a negative `nice`/`renice` and a
//! realtime `ionice` reach for more than they were given. Work handed
//! elsewhere leaves too: `wsl.exe` running a command (it starts in
//! `/init.scope`, outside the user manager), `ssh` to this machine, a
//! `docker`/`podman` container, `at`/`batch`/`crontab`. And `nohup`,
//! `setsid` and `disown` detach a command from the session that would
//! watch it.
//!
//! **A reminder, not a refusal**: hooks remind the model to play nicely
//! instead of Giverny taking charge. The command runs; the session is told, once per kind of
//! escape ([`Escape::kind`]), that it runs outside its tab's limits and
//! that heavy work goes through `giverny-manage run` or stays in the tab
//! ([`reminder`]). The slice's memory cap is the floor that holds whatever
//! is said. What is told is kept per session by [`crate::escapes`].
//!
//! **Executed words only**. A word is matched where a shell
//! would run it: quoted text, a heredoc's body, a commit message, a
//! `gh … -F body=@file` naming the tool, and a read of the slice's
//! cgroup files are not escapes. A heredoc fed to a shell (`bash <<EOF`)
//! is read as the commands it is.
//!
//! **Cheap.** It runs before every Bash call: one pass over the command's
//! text, no file read, no process started (`ssh` alone reads the host
//! name). It reads the shell the way a person would (quotes,
//! `;`/`&&`/`|`, `$(…)` and backticks, `sh -c '…'`, heredocs,
//! `env`/`sudo`/`timeout`/`xargs`/`nice` in front), not as bash does: a
//! command built at run time (`$(echo c3lz… | base64 -d)`), one a script
//! file holds, or one a program starts on its own is not seen — the app's
//! sampler finds those afterwards ([`crate::escapes::Sweep`]).

use crate::tab_cap::SLICE;

/// One way a command leaves its tab's cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escape {
    /// What it is, as a session is told it once: `systemd-run`, `cgroup`,
    /// `wsl`, `detach`, ….
    pub kind: &'static str,
    /// The words for it: "`systemd-run` (a scope or service of its own)".
    pub what: String,
}

impl Escape {
    fn new(kind: &'static str, what: impl Into<String>) -> Escape {
        Escape {
            kind,
            what: what.into(),
        }
    }

    /// It detaches a command rather than leaving the cap.
    pub fn detaches(&self) -> bool {
        self.kind == "detach"
    }
}

/// Every escape `cmd` makes, one per kind, in the order met.
pub fn escapes(cmd: &str) -> Vec<Escape> {
    let mut out = Vec::new();
    check_text(cmd, 0, &mut out);
    let mut seen = Vec::new();
    out.retain(|e| {
        let first = !seen.contains(&e.kind);
        seen.push(e.kind);
        first
    });
    out
}

/// The first escape `cmd` makes, said, or `None` when it makes none.
pub fn verdict(cmd: &str) -> Option<String> {
    let found = escapes(cmd);
    (!found.is_empty()).then(|| reminder(&found.iter().collect::<Vec<_>>()))
}

/// The line a session is shown for the escapes `found` its command makes.
pub fn reminder(found: &[&Escape]) -> String {
    reminder_on(found, cfg!(target_os = "macos"))
}

/// [`reminder`] as said on macOS (`macos`), where no slice holds the tab
/// but Giverny itself does ([`crate::user_cap`]), or on Linux.
pub fn reminder_on(found: &[&Escape], macos: bool) -> String {
    let held = if macos {
        "is held to the machine's limits by Giverny itself (no cgroups on macOS)".to_string()
    } else {
        format!("runs capped (in {SLICE})")
    };
    let said: Vec<String> = found
        .iter()
        .map(|e| {
            if e.detaches() {
                format!(
                    "{} detaches work from this session: it outlives the turn with nothing \
                     here watching what it uses",
                    e.what
                )
            } else {
                format!("{} runs outside this tab's limits", e.what)
            }
        })
        .collect();
    format!(
        "Giverny reminder (allowed, said once a session): {}. Every claude in a Giverny tab \
         {held}, and all it starts with it. Heavy work goes through `giverny-manage run \
         <task> -- <command>` (reserve RAM first with `giverny-manage claim <task> --ram \
         <size>`; CPU is shared and measured), or stays in the tab.",
        said.join("; ")
    )
}

/// How deep `sh -c '…'` inside `sh -c '…'` is followed.
pub(crate) const MAX_DEPTH: usize = 8;

fn check_text(cmd: &str, depth: usize, out: &mut Vec<Escape>) {
    if depth > MAX_DEPTH {
        return;
    }
    let parsed = split(cmd);
    for nested in &parsed.nested {
        check_text(nested, depth + 1, out);
    }
    for (k, seg) in parsed.segments.iter().enumerate() {
        if let Some(t) = seg.redirects.iter().find(|t| t.contains(CGROUP_FS)) {
            out.push(Escape::new("cgroup", format!("writing to `{t}`")));
        }
        check_words(&seg.words, depth, out);
        // A heredoc is a script only when a shell reads it: its own
        // command's, or the next one's (`cat <<EOF | bash`).
        let fed = |s: &Segment| reads_script_from_stdin(&s.words);
        if let Some(body) = &seg.heredoc
            && (fed(seg)
                || parsed
                    .segments
                    .get(k + 1)
                    .is_some_and(|n| n.heredoc.is_none() && fed(n)))
        {
            check_text(body, depth + 1, out);
        }
    }
}

/// Whether these words run a shell reading its script from stdin (`bash`,
/// `sh -s`, `sudo bash`), not one given `-c` or a script file.
fn reads_script_from_stdin(words: &[String]) -> bool {
    let mut i = 0;
    while i < words.len() && is_assignment(&words[i]) {
        i += 1;
    }
    let ws = &words[i..];
    let Some(first) = ws.first() else {
        return false;
    };
    let prog = base(first);
    let args = &ws[1..];
    if LEADERS.contains(&prog) || matches!(prog, "sudo" | "doas" | "env") {
        let from = match prog {
            "sudo" | "doas" => after_options(args, &["-u", "-g", "-C", "-p", "-D"]),
            "env" => args
                .iter()
                .position(|a| !a.starts_with('-') && !is_assignment(a))
                .unwrap_or(args.len()),
            _ => 0,
        };
        return reads_script_from_stdin(args.get(from..).unwrap_or_default());
    }
    if !SHELLS.contains(&prog) {
        return false;
    }
    let mut j = 0;
    while j < args.len() && args[j].starts_with('-') && args[j] != "-" {
        if !args[j].starts_with("--") && args[j][1..].contains('c') {
            return false;
        }
        j += 1;
    }
    j >= args.len() || args[j] == "-"
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

const CGROUP_FS: &str = "/sys/fs/cgroup";

/// One simple command: its words, and where its output is redirected.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) words: Vec<String>,
    pub(crate) redirects: Vec<String>,
    /// The text of its heredocs (`<<EOF … EOF`), when it has any: what it
    /// reads on stdin, never words of its own.
    pub(crate) heredoc: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub(crate) segments: Vec<Segment>,
    /// The text of each `$(…)` or backtick substitution met inside double
    /// quotes, checked as commands of their own.
    pub(crate) nested: Vec<String>,
}

/// What the word being read is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redir {
    /// `>`: a file written.
    Out,
    /// `<`, `<<<`: read, nothing written.
    In,
    /// `<<`, `<<-` (tabs stripped): the heredoc's delimiter.
    Heredoc(bool),
}

/// The command's simple commands, split the way a shell would at `;`,
/// `&`, `|`, newlines, parentheses and backticks, with quotes removed and
/// each heredoc's body kept apart from the words ([`Segment::heredoc`]).
pub(crate) fn split(cmd: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut seg = Segment::default();
    let mut word = String::new();
    // A word is open (even an empty one, as `''` is).
    let mut open = false;
    let mut redirect: Option<Redir> = None;
    // Heredocs whose bodies start after the next newline: delimiter, tabs
    // stripped, and the index its segment will have.
    let mut pending: Vec<(String, bool, usize)> = Vec::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    let end_word = |word: &mut String,
                    open: &mut bool,
                    seg: &mut Segment,
                    redirect: &mut Option<Redir>,
                    pending: &mut Vec<(String, bool, usize)>,
                    at: usize| {
        if *open {
            let w = std::mem::take(word);
            match redirect.take() {
                Some(Redir::Out) => seg.redirects.push(w),
                Some(Redir::In) => {}
                Some(Redir::Heredoc(strip)) => {
                    seg.heredoc.get_or_insert_with(String::new);
                    pending.push((w, strip, at));
                }
                None => seg.words.push(w),
            }
            *open = false;
        }
    };
    let end_seg = |seg: &mut Segment, out: &mut Parsed| {
        let s = std::mem::take(seg);
        if !s.words.is_empty() || !s.redirects.is_empty() || s.heredoc.is_some() {
            out.segments.push(s);
        }
    };

    macro_rules! end_word {
        () => {
            end_word(
                &mut word,
                &mut open,
                &mut seg,
                &mut redirect,
                &mut pending,
                out.segments.len(),
            )
        };
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => end_word!(),
            ';' | '\n' | '&' | '|' | '(' | ')' | '`' => {
                end_word!();
                redirect = None;
                end_seg(&mut seg, &mut out);
                if c == '\n' && !pending.is_empty() {
                    i = heredoc_bodies(&chars, i + 1, &mut pending, &mut out);
                    continue;
                }
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                end_word!();
                redirect = None;
                end_seg(&mut seg, &mut out);
                i += 1;
            }
            '#' if !open => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '>' | '<' => {
                // `2>`, `&>`: the number is the stream, not a word.
                if open && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    open = false;
                }
                end_word!();
                if c == '<' && chars.get(i + 1) == Some(&'<') {
                    i += 1;
                    if chars.get(i + 1) == Some(&'<') {
                        // `<<<word`: a here-string, read.
                        i += 1;
                        redirect = Some(Redir::In);
                    } else {
                        let strip = chars.get(i + 1) == Some(&'-');
                        if strip {
                            i += 1;
                        }
                        redirect = Some(Redir::Heredoc(strip));
                    }
                } else {
                    while matches!(chars.get(i + 1), Some('>' | '&' | '|')) {
                        i += 1;
                    }
                    redirect = Some(if c == '>' { Redir::Out } else { Redir::In });
                }
            }
            '\\' => {
                if let Some(&n) = chars.get(i + 1) {
                    if n != '\n' {
                        word.push(n);
                        open = true;
                    }
                    i += 1;
                }
            }
            '\'' => {
                open = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                open = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`')) => {
                            word.push(chars[i + 1]);
                            i += 1;
                        }
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            let (inner, next) = until_close(&chars, i + 2, '(', ')');
                            out.nested.push(inner);
                            i = next;
                        }
                        '`' => {
                            let (inner, next) = until_close(&chars, i + 1, '\0', '`');
                            out.nested.push(inner);
                            i = next;
                        }
                        ch => word.push(ch),
                    }
                    i += 1;
                }
            }
            ch => {
                word.push(ch);
                open = true;
            }
        }
        i += 1;
    }
    end_word!();
    end_seg(&mut seg, &mut out);
    out
}

/// Read the bodies of the `pending` heredocs, one after another, from
/// `from` (a line's start) into their segments; where the text after the
/// last one starts.
fn heredoc_bodies(
    chars: &[char],
    from: usize,
    pending: &mut Vec<(String, bool, usize)>,
    out: &mut Parsed,
) -> usize {
    let mut i = from;
    for (delim, strip, at) in pending.drain(..) {
        let mut body = String::new();
        while i < chars.len() {
            let end = chars[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |n| i + n);
            let line: String = chars[i..end].iter().collect();
            i = (end + 1).min(chars.len());
            let shown = if strip {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if shown == delim {
                break;
            }
            body.push_str(shown);
            body.push('\n');
        }
        if let Some(seg) = out.segments.get_mut(at) {
            seg.heredoc.get_or_insert_with(String::new).push_str(&body);
        }
    }
    i
}

/// The text from `from` up to the `close` that ends it (nested `open`s
/// counted), and the index of that `close`.
fn until_close(chars: &[char], from: usize, open: char, close: char) -> (String, usize) {
    let mut depth = 0usize;
    let mut i = from;
    let mut s = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == close && depth == 0 {
            return (s, i);
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
        }
        s.push(c);
        i += 1;
    }
    (s, i)
}

/// Programs that change what they are given: a path under
/// `/sys/fs/cgroup` in their arguments is a write (a move, a limit, a new
/// cgroup). Any other program naming one is reading it.
const WRITERS: &[&str] = &[
    "tee", "cp", "mv", "dd", "install", "ln", "truncate", "rm", "rmdir", "mkdir", "chmod", "chown",
    "chgrp", "touch", "rsync",
];

/// Words that only lead into the command after them.
pub(crate) const LEADERS: &[&str] = &[
    "exec", "command", "builtin", "time", "nohup", "setsid", "then", "do", "else", "elif", "if",
    "while", "until", "!", "{", "}", "unbuffer", "chronic",
];

pub(crate) fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

pub(crate) fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A negative niceness among `nice`'s options: `-n -5`, `-n-5`,
/// `--adjustment=-5`, `--5`.
fn negative_nice(args: &[String]) -> bool {
    let neg = |v: &str| v.trim().parse::<i64>().is_ok_and(|n| n < 0);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-n" || a == "--adjustment" {
            if args.get(i + 1).is_some_and(|v| neg(v)) {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(v) = a
            .strip_prefix("--adjustment=")
            .or_else(|| a.strip_prefix("-n"))
            && neg(v)
        {
            return true;
        }
        // `nice --5 cmd` is an adjustment of -5.
        if a.starts_with("--") && a[2..].parse::<u64>().is_ok() {
            return true;
        }
        i += 1;
    }
    false
}

/// Where the command after `nice`'s options starts.
pub(crate) fn after_nice(args: &[String]) -> usize {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-n" || a == "--adjustment" {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// `renice`'s new priority, when it is negative: `-n -5`, `-5`, `--priority -5`.
fn negative_renice(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-n" | "--priority" | "--relative") {
            return args
                .get(i + 1)
                .is_some_and(|v| v.parse::<i64>().is_ok_and(|n| n < 0));
        }
        if let Ok(n) = a.parse::<i64>() {
            return n < 0;
        }
        i += 1;
    }
    false
}

/// `ionice -c 1` (realtime).
fn realtime_ionice(args: &[String]) -> bool {
    let rt = |v: &str| matches!(v.trim(), "1" | "realtime");
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-c" || a == "--class" {
            if args.get(i + 1).is_some_and(|v| rt(v)) {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("-c").or_else(|| a.strip_prefix("--class="))
            && rt(v)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Skip `prog`'s options (those in `with_value` take the next word) and
/// return where its command starts.
pub(crate) fn after_options(args: &[String], with_value: &[&str]) -> usize {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            return i + 1;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        i += if with_value.contains(&a) { 2 } else { 1 };
    }
    i
}

/// Whether `wsl.exe args` runs a command (or a shell) in a distro, rather
/// than managing WSL (`--list`, `--status`, `--shutdown`, …).
fn wsl_runs(args: &[String]) -> bool {
    const RUN_OPTS: &[&str] = &[
        "-d",
        "--distribution",
        "-u",
        "--user",
        "--cd",
        "--shell-type",
        "--distribution-id",
    ];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-e" | "--exec" | "--") {
            return true;
        }
        if RUN_OPTS.contains(&a) {
            i += 2;
            continue;
        }
        if a == "--system" || a == "~" {
            i += 1;
            continue;
        }
        // Any other option manages WSL; a word is the command to run.
        return !a.starts_with('-');
    }
    // `wsl.exe` alone, or with only a distro or user: a login shell.
    true
}

/// The name this machine goes by (`/proc/sys/kernel/hostname`), read
/// only when an `ssh` is met.
fn own_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
}

/// Whether `ssh args` goes to this machine.
fn ssh_local(args: &[String], hostname: impl FnOnce() -> Option<String>) -> bool {
    const WITH_VALUE: &[&str] = &[
        "-p", "-i", "-l", "-o", "-F", "-J", "-L", "-R", "-D", "-b", "-c", "-E", "-e", "-I", "-m",
        "-O", "-Q", "-S", "-W", "-w", "-B",
    ];
    let at = after_options(args, WITH_VALUE);
    let Some(target) = args.get(at) else {
        return false;
    };
    let t = target.strip_prefix("ssh://").unwrap_or(target);
    let t = t.rsplit('@').next().unwrap_or(t);
    let host = if let Some(v6) = t.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else if t.matches(':').count() > 1 {
        t // a bare IPv6 address
    } else {
        t.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    matches!(
        host.as_str(),
        "localhost" | "::1" | "0.0.0.0" | "ip6-localhost"
    ) || host.starts_with("127.")
        || host.ends_with(".localhost")
        || hostname().is_some_and(|h| {
            let short = h.split('.').next().unwrap_or(&h);
            [h.as_str(), short].iter().any(|n| {
                host == *n || host == format!("{n}.local") || host == format!("{n}.localdomain")
            })
        })
}

/// The container verb `docker`/`podman args` runs, when it starts or runs
/// something: `run`, `exec`, `compose up`, `buildx build`, ….
fn container_verb(args: &[String]) -> Option<String> {
    const WITH_VALUE: &[&str] = &[
        "-H",
        "--host",
        "-c",
        "--context",
        "--config",
        "-l",
        "--log-level",
        "--url",
        "--connection",
    ];
    const RUNS: &[&str] = &["run", "exec", "start", "build", "create", "restart"];
    let words: Vec<&str> = {
        let at = after_options(args, WITH_VALUE);
        args[at.min(args.len())..]
            .iter()
            .filter(|a| !a.starts_with('-'))
            .map(String::as_str)
            .take(2)
            .collect()
    };
    match words.as_slice() {
        [v, ..] if RUNS.contains(v) => Some(v.to_string()),
        ["compose", v] if matches!(*v, "up" | "run" | "start" | "exec" | "restart") => {
            Some(format!("compose {v}"))
        }
        ["buildx" | "builder", "build"] => Some(format!("{} build", words[0])),
        ["container", v] if RUNS.contains(v) => Some(format!("container {v}")),
        ["image", "build"] => Some("image build".into()),
        _ => None,
    }
}

fn check_words(words: &[String], depth: usize, out: &mut Vec<Escape>) {
    let mut i = 0;
    while i < words.len() && is_assignment(&words[i]) {
        i += 1;
    }
    let words = &words[i..];
    let Some(first) = words.first() else {
        return;
    };
    let prog = base(first);
    let args = &words[1..];
    let rest = |from: usize, out: &mut Vec<Escape>| {
        check_words(args.get(from..).unwrap_or_default(), depth, out)
    };

    if matches!(prog, "nohup" | "setsid") {
        out.push(Escape::new("detach", format!("`{prog}`")));
        let from = if prog == "setsid" {
            after_options(args, &[])
        } else {
            0
        };
        return rest(from, out);
    }
    if LEADERS.contains(&prog) {
        return rest(0, out);
    }
    match prog {
        "disown" => {
            out.push(Escape::new("detach", "`disown`"));
            return;
        }
        "sudo" | "doas" => return rest(after_options(args, &["-u", "-g", "-C", "-p", "-D"]), out),
        "env" => {
            let mut j = 0;
            while j < args.len() {
                let a = args[j].as_str();
                if a == "-S" || a == "--split-string" {
                    let inner = args.get(j + 1).cloned().unwrap_or_default();
                    let tail = args.get(j + 2..).unwrap_or_default().join(" ");
                    return check_text(&format!("{inner} {tail}"), depth + 1, out);
                }
                if matches!(a, "-u" | "--unset" | "-C" | "--chdir") {
                    j += 2;
                } else if a.starts_with('-') || is_assignment(a) {
                    j += 1;
                } else {
                    break;
                }
            }
            return rest(j, out);
        }
        "timeout" => {
            let j = after_options(args, &["-s", "--signal", "-k", "--kill-after"]);
            return rest(j + 1, out); // the duration
        }
        "xargs" => {
            return rest(
                after_options(args, &["-I", "-n", "-P", "-L", "-s", "-d", "-E", "-a"]),
                out,
            );
        }
        "stdbuf" | "watch" => return rest(after_options(args, &["-n", "-i", "-o", "-e"]), out),
        "flock" => {
            let j = after_options(args, &["-w", "--timeout", "-E", "--conflict-exit-code"]);
            // `flock FILE -c 'cmd'` or `flock FILE cmd …`.
            if args
                .get(j + 1)
                .is_some_and(|a| a == "-c" || a == "--command")
            {
                return check_text(args.get(j + 2).map_or("", String::as_str), depth + 1, out);
            }
            return rest(j + 1, out);
        }
        "nice" => {
            if negative_nice(args) {
                out.push(Escape::new("nice", "`nice` with a negative adjustment"));
            }
            return rest(after_nice(args), out);
        }
        "renice" => {
            if negative_renice(args) {
                out.push(Escape::new("renice", "`renice` to a higher priority"));
            }
            return;
        }
        "ionice" => {
            if realtime_ionice(args) {
                out.push(Escape::new("ionice", "`ionice` realtime"));
            }
            if args.iter().any(|a| a == "-p" || a == "-P" || a == "-u") {
                return;
            }
            return rest(
                after_options(args, &["-c", "-n", "--class", "--classdata"]),
                out,
            );
        }
        _ if SHELLS.contains(&prog) => {
            let mut j = 0;
            let mut script = false;
            while j < args.len() && args[j].starts_with('-') && args[j] != "-" {
                if !args[j].starts_with("--") && args[j][1..].contains('c') {
                    script = true;
                }
                j += 1;
            }
            if script {
                check_text(args.get(j).map_or("", String::as_str), depth + 1, out);
            } // else a script file, or stdin: not seen here
            return;
        }
        "eval" => return check_text(&args.join(" "), depth + 1, out),
        "giverny-manage" => return through_run(args, depth, out),
        "giverny" if args.first().is_some_and(|a| a == "manage") => {
            return through_run(&args[1..], depth, out);
        }
        _ => {}
    }

    match prog {
        "systemd-run" => {
            if !args
                .iter()
                .all(|a| matches!(a.as_str(), "--help" | "-h" | "--version"))
            {
                out.push(Escape::new(
                    "systemd-run",
                    "`systemd-run` (a scope or service of its own)",
                ));
            }
        }
        "systemctl" => {
            const LIFTS: &[&str] = &[
                "set-property",
                "revert",
                "edit",
                "start",
                "restart",
                "try-restart",
                "reload-or-restart",
                "try-reload-or-restart",
                "isolate",
                "link",
                "daemon-reexec",
            ];
            let verb = args.iter().find(|a| !a.starts_with('-'));
            if let Some(v) = verb
                && LIFTS.contains(&v.as_str())
            {
                out.push(Escape::new("systemctl", format!("`systemctl {v}`")));
            } else if verb.is_some_and(|v| v == "enable" || v == "reenable")
                && args.iter().any(|a| a == "--now")
            {
                out.push(Escape::new("systemctl", "`systemctl enable --now`"));
            }
        }
        "cgexec" | "cgclassify" | "cgcreate" | "cgset" | "cgdelete" => {
            out.push(Escape::new("cgtools", format!("`{prog}`")));
        }
        "taskset" => {
            // `taskset -p PID` only reads the affinity.
            let reads = args.len() == 2 && args[0] == "-p" && args[1].parse::<u32>().is_ok();
            if !reads {
                out.push(Escape::new("taskset", "`taskset` (CPU affinity)"));
            }
        }
        "chrt" | "numactl" => out.push(Escape::new("sched", format!("`{prog}` (scheduling)"))),
        "busctl" | "dbus-send" | "gdbus" => {
            const CALLS: &[&str] = &[
                "StartTransientUnit",
                "AttachProcessesToUnit",
                "SetUnitProperties",
                "StartUnit",
                "RestartUnit",
            ];
            if let Some(c) = CALLS.iter().find(|c| args.iter().any(|a| a.contains(*c))) {
                out.push(Escape::new("dbus", format!("a systemd `{c}` call")));
            }
        }
        "wsl.exe" | "wsl" => {
            if wsl_runs(args) {
                out.push(Escape::new(
                    "wsl",
                    "`wsl.exe` running a command (it starts in /init.scope, outside every cap)",
                ));
            }
        }
        "ssh" => {
            if ssh_local(args, own_hostname) {
                out.push(Escape::new(
                    "ssh-local",
                    "`ssh` to this machine (its command runs under sshd, not the tab)",
                ));
            }
        }
        "docker" | "podman" | "nerdctl" | "docker.exe" | "podman.exe" => {
            if let Some(v) = container_verb(args) {
                out.push(Escape::new(
                    "container",
                    format!(
                        "`{} {v}` (the container runs outside the tab)",
                        prog.trim_end_matches(".exe")
                    ),
                ));
            }
        }
        "at" | "batch" => {
            let manages = args
                .iter()
                .any(|a| matches!(a.as_str(), "-l" | "-r" | "-d" | "-c" | "-V" | "--help"));
            if !manages {
                out.push(Escape::new(
                    "at",
                    format!("`{prog}` (the job runs from atd, outside the tab)"),
                ));
            }
        }
        "crontab" => {
            let mut j = 0;
            let mut installs = false;
            while j < args.len() {
                match args[j].as_str() {
                    "-u" => j += 1,
                    "-l" | "-r" | "-i" | "-V" | "--help" => {}
                    _ => installs = true, // `-e`, a file, `-`
                }
                j += 1;
            }
            if installs {
                out.push(Escape::new(
                    "cron",
                    "`crontab` (its jobs run from cron, outside the tab)",
                ));
            }
        }
        _ => {}
    }

    if let Some(p) = args.iter().find(|a| a.contains(CGROUP_FS)) {
        let writes = WRITERS.contains(&prog)
            || (matches!(prog, "sed" | "perl")
                && args
                    .iter()
                    .any(|a| a.starts_with("-i") || a == "--in-place"));
        if writes {
            out.push(Escape::new("cgroup", format!("`{prog}` on `{p}`")));
        }
    }
}

/// `giverny manage <sub> …`: a `run` passes, and what it runs is checked.
fn through_run(args: &[String], depth: usize, out: &mut Vec<Escape>) {
    if args.first().is_none_or(|a| a != "run") {
        return;
    }
    if let Some(dash) = args.iter().position(|a| a == "--") {
        check_words(&args[dash + 1..], depth, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escapes_of(cmd: &str) -> bool {
        !escapes(cmd).is_empty()
    }

    #[test]
    fn lifting_a_cap_or_reaching_for_more_is_an_escape() {
        for cmd in [
            "systemctl --user set-property giverny-claude.slice CPUQuota=",
            "systemctl --user revert giverny-claude.slice",
            "systemctl --user start my-build.service",
            "systemctl --user enable --now my-build.service",
            "cgexec -g cpu:/ make",
            "taskset -c 0-13 make",
            "taskset -p 0xff 1234",
            "chrt -f 10 make",
            "nice -n -5 make",
            "nice --adjustment=-5 make",
            "nice --5 make",
            "renice -n -5 -p 1234",
            "renice -5 1234",
            "ionice -c 1 make",
            "echo 1234 > /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/cgroup.procs",
            "echo max >> /sys/fs/cgroup/x/cpu.max",
            "echo 1234 | tee /sys/fs/cgroup/x/cgroup.procs",
            "sed -i s/3/9/ /sys/fs/cgroup/x/cpu.max",
            "mkdir /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/mine",
            "busctl --user call org.freedesktop.systemd1 /org/freedesktop/systemd1 \
             org.freedesktop.systemd1.Manager StartTransientUnit ssa(sv)a(sa(sv)) x.scope fail 0 0",
            "giverny-manage run t -- systemd-run --user --scope make",
        ] {
            assert!(escapes_of(cmd), "{cmd}");
        }
    }
}
