//! Shell history per tab.
//!
//! Each tab's shell keeps its own history file, named by the tab's stable
//! [`TabId`] and kept beside its restore snapshot, so a restored tab's ↑
//! gives that tab's own last commands rather than whatever any shell on the
//! machine ran last.
//!
//! No rc file of the user's is written, and a shell started anywhere else is
//! untouched. The tab's shell is started through a small init file of
//! Giverny's own ([`write_init`], under `state/shell/`) that runs the user's
//! usual startup files and sets the history *inside* the shell, unexported:
//! a shell started from the tab — a nested bash, a zsh in a bash tab
//! — gets none of it and keeps its own defaults. The startup files run after
//! the tab's file is named, so an rc that sets `HISTFILE` itself wins, and
//! the tab simply has the user's usual history, as before.
//!
//! - **bash**: `--rcfile` ours, which names the file, runs `~/.bashrc`
//!   (`/etc/bash.bashrc` too, where bash reads it), then adds `history -a` to
//!   whatever `PROMPT_COMMAND` the rc left — so a killed tab keeps what it
//!   ran, even under an rc (starship and the like) that sets
//!   `PROMPT_COMMAND` outright. The hook writes nothing once `HISTFILE` is
//!   not the tab's. With `also_shared`, each new entry is appended to
//!   `~/.bash_history` too.
//! - **zsh**: `ZDOTDIR` pointed at ours, whose `.zshenv` puts the user's
//!   `ZDOTDIR` straight back, names the file (and `SAVEHIST`/`HISTSIZE`
//!   where the environment did not, since zsh saves nothing by default),
//!   then runs the user's `.zshenv`; the rest of zsh's startup is the
//!   user's. Written when the shell exits: per-command writes are a
//!   `setopt`, left to the user. `also_shared` does not apply.
//! - **fish**: `--init-command`, after `config.fish`, sets a `fish_history`
//!   session of the tab's own where the config set none; fish writes every
//!   command as it runs. `also_shared` does not apply.
//! - Anything else (sh, PowerShell, cmd, a WSL shell), or a shell already
//!   given arguments of its own: left alone.

use std::path::{Path, PathBuf};

use crate::state::Paths;
use crate::tabs::TabId;

/// The shells whose history can be steered from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl ShellKind {
    /// Which shell a program path runs, by its file name; `None` for one this
    /// does not know how to steer.
    pub fn of(program: &str) -> Option<ShellKind> {
        let name = Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program);
        let name = name.strip_suffix(".exe").unwrap_or(name);
        match name {
            "bash" => Some(ShellKind::Bash),
            "zsh" => Some(ShellKind::Zsh),
            "fish" => Some(ShellKind::Fish),
            _ => None,
        }
    }
}

/// Where one tab's history lives: `state/history/<tab>`, beside its snapshot.
pub fn history_file(paths: &Paths, tab: TabId) -> PathBuf {
    paths
        .base()
        .join("state")
        .join("history")
        .join(tab.0.to_string())
}

/// fish keeps history by session name, in its own data directory.
fn fish_session(tab: TabId) -> String {
    format!("giverny_{}", tab.0)
}

/// The file fish writes for a tab's session.
fn fish_file(tab: TabId) -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))?;
    Some(
        data.join("fish")
            .join(format!("{}_history", fish_session(tab))),
    )
}

/// Where Giverny keeps the init files it starts shells with.
pub fn init_dir(paths: &Paths) -> PathBuf {
    paths.base().join("state").join("shell")
}

/// bash's `--rcfile`: name the tab's file, run what bash would have run
/// without us, then hook `history -a` onto whatever prompt command that left.
///
/// The variables Giverny passes in are taken in and unset first, so nothing
/// of this reaches a shell started from the tab. The prompt-command entry
/// expands to nothing where `__giverny_hf` is unset, so even an inherited,
/// exported `PROMPT_COMMAND` carries nothing a nested shell trips over.
pub const BASHRC: &str = r#"# Written by Giverny for its terminal tabs (per-tab history) and rewritten
# when a tab starts: edits here do not last. It runs your ~/.bashrc;
# nothing of yours is changed.
__giverny_hf=${GIVERNY_HISTFILE-}
__giverny_hf_shared=${GIVERNY_HISTFILE_SHARED-}
unset GIVERNY_HISTFILE GIVERNY_HISTFILE_SHARED
if [ -n "$__giverny_hf" ]; then
    export -n HISTFILE
    HISTFILE=$__giverny_hf
fi
# What bash reads without --rcfile; an HISTFILE set there wins over the tab's.
if [ -f ~/.bashrc ]; then . ~/.bashrc; fi
# Write this tab's new entries at every prompt, not only at exit.
__giverny_history() {
    [ -n "$__giverny_hf" ] && [ "${HISTFILE-}" = "$__giverny_hf" ] || return 0
    if [ -n "$__giverny_hf_shared" ]; then
        local n
        n=$(wc -l 2>/dev/null <"$HISTFILE")
        history -a
        tail -n "+$((n + 1))" "$HISTFILE" >>"$__giverny_hf_shared" 2>/dev/null
    else
        history -a
    fi
}
if [ -n "$__giverny_hf" ]; then
    if [ -n "${PROMPT_COMMAND-}" ]; then
        PROMPT_COMMAND+=$'\n''${__giverny_hf:+__giverny_history}'
    else
        PROMPT_COMMAND='${__giverny_hf:+__giverny_history}'
    fi
fi
# Tell the tab where each command starts and how it ended, which bash sends
# no terminal by itself (OSC 133 C, and D with the exit code): C as the
# command runs (PS0), D first thing at the next prompt, ahead of the rest of
# PROMPT_COMMAND so it reads the command's own $?, which it hands on. Both
# go where bash writes its prompts.
__giverny_mark_done() {
    local s=$?
    builtin printf '\e]133;D;%s\a' "$s" >&2
    return "$s"
}
if [ -n "$__giverny_hf" ]; then
    PS0+='${__giverny_hf:+\e]133;C\a}'
    PROMPT_COMMAND='${__giverny_hf:+__giverny_mark_done}'$'\n'"$PROMPT_COMMAND"
fi
"#;

/// zsh's `.zshenv` under the `ZDOTDIR` Giverny starts it with: put the
/// user's `ZDOTDIR` back at once — so `.zprofile`, `.zshrc`, `.zlogin` and
/// every shell started from the tab are the user's own — name the tab's
/// file, and run the user's `.zshenv`.
pub const ZSHENV: &str = r#"# Written by Giverny for its terminal tabs (per-tab history) and rewritten
# when a tab starts: edits here do not last. It runs your own .zshenv;
# nothing of yours is changed.
if [[ -n ${GIVERNY_ZDOTDIR+x} ]]; then
    ZDOTDIR=$GIVERNY_ZDOTDIR
else
    unset ZDOTDIR
fi
__giverny_hf=${GIVERNY_HISTFILE-}
unset GIVERNY_ZDOTDIR GIVERNY_HISTFILE
if [[ -n $__giverny_hf ]]; then
    typeset +x HISTFILE
    HISTFILE=$__giverny_hf
    # zsh keeps 30 and saves none; sizes from the environment are the user's.
    [[ ${(t)HISTSIZE} == *export* ]] || HISTSIZE=10000
    [[ ${(t)SAVEHIST} == *export* ]] || SAVEHIST=10000
fi
unset __giverny_hf
if [[ -f ${ZDOTDIR:-$HOME}/.zshenv ]]; then
    source "${ZDOTDIR:-$HOME}/.zshenv"
fi
"#;

/// The init files, as `(path under the init dir, contents)`.
const INIT_FILES: [(&str, &str); 2] = [("bashrc", BASHRC), ("zsh/.zshenv", ZSHENV)];

/// Put the init files in `dir`, rewriting only those that differ.
pub fn write_init(dir: &Path) -> std::io::Result<()> {
    for (name, contents) in INIT_FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|now| now == contents) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

/// How a tab's shell is started so it keeps its own history: arguments to
/// start it with, and environment for the init file to take in. Nothing in
/// `env` is one of the shell's own history variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Steer {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What gives a tab's shell its own history, its init files in `init` (see
/// [`write_init`]).
///
/// `inherited` looks up a variable in the environment the shell would
/// otherwise get — zsh's `ZDOTDIR`, which the init file puts back.
pub fn steer(
    kind: ShellKind,
    file: &Path,
    tab: TabId,
    also_shared: bool,
    home: Option<&Path>,
    init: &Path,
    inherited: impl Fn(&str) -> Option<String>,
) -> Steer {
    let file = file.display().to_string();
    match kind {
        ShellKind::Bash => {
            let mut env = vec![("GIVERNY_HISTFILE".to_string(), file)];
            if also_shared && let Some(home) = home {
                env.push((
                    "GIVERNY_HISTFILE_SHARED".to_string(),
                    home.join(".bash_history").display().to_string(),
                ));
            }
            Steer {
                args: vec![
                    "--rcfile".to_string(),
                    init.join("bashrc").display().to_string(),
                ],
                env,
            }
        }
        ShellKind::Zsh => {
            let mut env = vec![
                (
                    "ZDOTDIR".to_string(),
                    init.join("zsh").display().to_string(),
                ),
                ("GIVERNY_HISTFILE".to_string(), file),
            ];
            if let Some(user) = inherited("ZDOTDIR") {
                env.push(("GIVERNY_ZDOTDIR".to_string(), user));
            }
            Steer {
                args: Vec::new(),
                env,
            }
        }
        ShellKind::Fish => Steer {
            args: vec![
                "--init-command".to_string(),
                format!(
                    "set -q fish_history; or set -g fish_history {}",
                    fish_session(tab)
                ),
            ],
            env: Vec::new(),
        },
    }
}

/// A tab closed for good takes its history with it.
pub fn remove(paths: &Paths, tab: TabId) {
    let _ = std::fs::remove_file(history_file(paths, tab));
    if let Some(fish) = fish_file(tab) {
        let _ = std::fs::remove_file(fish);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn shells_are_known_by_file_name() {
        assert_eq!(ShellKind::of("/bin/bash"), Some(ShellKind::Bash));
        assert_eq!(ShellKind::of("/usr/bin/zsh"), Some(ShellKind::Zsh));
        assert_eq!(ShellKind::of("/usr/local/bin/fish"), Some(ShellKind::Fish));
        assert_eq!(ShellKind::of("bash"), Some(ShellKind::Bash));
        assert_eq!(ShellKind::of("/bin/sh"), None);
        assert_eq!(ShellKind::of("pwsh.exe"), None);
        assert_eq!(ShellKind::of("wsl.exe"), None);
    }

    #[test]
    fn each_tab_has_its_own_file_beside_the_snapshots() {
        let paths = Paths::at("/cfg/giverny");
        assert_eq!(
            history_file(&paths, TabId(7)),
            PathBuf::from("/cfg/giverny/state/history/7")
        );
        assert_ne!(
            history_file(&paths, TabId(7)),
            history_file(&paths, TabId(8))
        );
    }

    /// The tab's history variables, which no shell started from the tab
    /// may inherit.
    const HISTORY_VARS: [&str; 5] = [
        "HISTFILE",
        "PROMPT_COMMAND",
        "HISTSIZE",
        "SAVEHIST",
        "fish_history",
    ];

    fn steer_of(kind: ShellKind, also_shared: bool, inherited: Option<(&str, &str)>) -> Steer {
        steer(
            kind,
            Path::new("/s/history/3"),
            TabId(3),
            also_shared,
            Some(Path::new("/h")),
            Path::new("/s/shell"),
            |k| {
                inherited
                    .filter(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            },
        )
    }

    /// A scratch directory of the test's own, with the init files in it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-hist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_init(&dir.join("init")).unwrap();
        dir
    }

    /// The first program of that name on `PATH`.
    #[cfg(unix)]
    fn on_path(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    /// Run `shell` interactively, steered, with `HOME` at `home` and nothing
    /// else of this process's environment, feeding it `input`.
    /// What the shell wrote to its stderr, where an interactive shell puts
    /// its prompts.
    #[cfg(unix)]
    fn run_steered(shell: &Path, steer: &Steer, home: &Path, input: &str) -> String {
        use std::io::Write;
        let mut cmd = std::process::Command::new(shell);
        cmd.args(&steer.args)
            .arg("-i")
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(steer.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// The init file marks each command for the tab: OSC 133 `C` as it runs
    /// and `D` with its exit code at the next prompt, ahead of an rc's own
    /// prompt command, which still gets the command's `$?`. An empty Enter
    /// runs nothing and marks no start.
    #[cfg(unix)]
    #[test]
    fn bash_marks_each_command_with_its_exit_code() {
        let Some(bash) = on_path("bash") else { return };
        let dir = scratch("bash-marks");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "PROMPT_COMMAND='echo \"rc saw $?\" >&2'\n",
        )
        .unwrap();
        let steer = steer(
            ShellKind::Bash,
            &dir.join("tab"),
            TabId(1),
            false,
            Some(&home),
            &dir.join("init"),
            |_| None,
        );
        let out = run_steered(&bash, &steer, &home, "false\n\ntrue\n(exit 130)\nexit\n");
        let marks: Vec<&str> = out
            .split('\x1b')
            .filter_map(|s| s.strip_prefix("]133;"))
            .map(|s| s.split('\x07').next().unwrap_or_default())
            .collect();
        // The first prompt's D is the rc's own status, before any command.
        assert_eq!(
            marks,
            ["D;0", "C", "D;1", "D;1", "C", "D;0", "C", "D;130", "C"],
            "{out:?}"
        );
        assert!(out.contains("rc saw 1"), "{out:?}");
        assert!(out.contains("rc saw 130"), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The history variables in an `env` dump.
    #[cfg(unix)]
    fn leaked(dump: &Path) -> Vec<String> {
        std::fs::read_to_string(dump)
            .unwrap()
            .lines()
            .filter(|l| {
                let name = l.split('=').next().unwrap_or_default();
                HISTORY_VARS.contains(&name) || name.starts_with("GIVERNY_HISTFILE")
            })
            .map(str::to_string)
            .collect()
    }

    /// The init file, run by a real interactive bash under an rc that sets
    /// `PROMPT_COMMAND` outright: per-command writes, nothing for a child.
    #[cfg(unix)]
    #[test]
    fn bash_writes_per_command_and_children_inherit_nothing() {
        let Some(bash) = on_path("bash") else { return };
        let dir = scratch("bash");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".bashrc"), "PROMPT_COMMAND='echo hi'\n").unwrap();
        let shared = home.join(".bash_history");
        std::fs::write(&shared, "older\n").unwrap();
        let run = |also_shared: bool, file: &Path| {
            let steer = steer(
                ShellKind::Bash,
                file,
                TabId(1),
                also_shared,
                Some(&home),
                &dir.join("init"),
                |_| None,
            );
            // Killed, not exited: what is in the file got there per command.
            let input = format!(
                "echo one\nenv >{}\necho two\nkill -9 $$\n",
                dir.join("env").display()
            );
            run_steered(&bash, &steer, &home, &input);
        };

        let a = dir.join("a");
        run(false, &a);
        let env_line = format!("env >{}\n", dir.join("env").display());
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            format!("echo one\n{env_line}echo two\n")
        );
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "older\n");
        assert_eq!(leaked(&dir.join("env")), Vec::<String>::new());

        let b = dir.join("b");
        run(true, &b);
        assert_eq!(
            std::fs::read_to_string(&b).unwrap(),
            format!("echo one\n{env_line}echo two\n")
        );
        assert_eq!(
            std::fs::read_to_string(&shared).unwrap(),
            format!("older\necho one\n{env_line}echo two\n")
        );
        assert_eq!(leaked(&dir.join("env")), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An rc that names its own `HISTFILE` wins, and the hook then writes
    /// nothing to the tab's file.
    #[cfg(unix)]
    #[test]
    fn bash_rc_histfile_wins() {
        let Some(bash) = on_path("bash") else { return };
        let dir = scratch("bash-rc");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let own = dir.join("own");
        std::fs::write(
            home.join(".bashrc"),
            format!("HISTFILE={}\n", own.display()),
        )
        .unwrap();
        let tab = dir.join("tab");
        let steer = steer(
            ShellKind::Bash,
            &tab,
            TabId(1),
            false,
            Some(&home),
            &dir.join("init"),
            |_| None,
        );
        run_steered(&bash, &steer, &home, "echo one\nexit\n");
        assert!(!tab.exists(), "the tab's file is untouched");
        assert!(std::fs::read_to_string(&own).unwrap().contains("echo one"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `.zshenv`, run by a real zsh where one is installed: the tab's
    /// file is written, the user's `ZDOTDIR` and startup files are theirs
    /// again, and a child gets no history variable.
    #[cfg(unix)]
    #[test]
    fn zsh_keeps_its_file_and_gives_children_nothing() {
        let Some(zsh) = on_path("zsh") else { return };
        let dir = scratch("zsh");
        let home = dir.join("home");
        let zdot = home.join("zd");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::write(
            zdot.join(".zshrc"),
            format!("print -r -- \"$ZDOTDIR\" >{}\n", dir.join("rc").display()),
        )
        .unwrap();
        let tab = dir.join("tab");
        let zdot_s = zdot.display().to_string();
        // The user's own ZDOTDIR, which the tab's environment would carry.
        let steer = steer(
            ShellKind::Zsh,
            &tab,
            TabId(1),
            false,
            Some(&home),
            &dir.join("init"),
            |k| (k == "ZDOTDIR").then(|| zdot_s.clone()),
        );
        let input = format!("print one\nenv >{}\nexit\n", dir.join("env").display());
        run_steered(&zsh, &steer, &home, &input);
        assert!(
            std::fs::read_to_string(&tab)
                .unwrap()
                .starts_with("print one\n")
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("rc")).unwrap().trim(),
            zdot_s,
            "the user's .zshrc ran, under their ZDOTDIR"
        );
        assert_eq!(leaked(&dir.join("env")), Vec::<String>::new());
        let env = std::fs::read_to_string(dir.join("env")).unwrap();
        assert!(env.contains(&format!("ZDOTDIR={zdot_s}\n")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_starts_on_the_init_file() {
        let s = steer_of(ShellKind::Bash, false, None);
        assert_eq!(s.args, vec!["--rcfile", "/s/shell/bashrc"]);
        assert_eq!(get(&s.env, "GIVERNY_HISTFILE"), Some("/s/history/3"));
        assert_eq!(
            get(&s.env, "GIVERNY_HISTFILE_SHARED"),
            None,
            "off by default"
        );
    }

    #[test]
    fn bash_shared_appends_to_the_usual_file() {
        let s = steer_of(ShellKind::Bash, true, None);
        assert_eq!(
            get(&s.env, "GIVERNY_HISTFILE_SHARED"),
            Some("/h/.bash_history")
        );
        // Append only: the usual file is never truncated or rewritten.
        assert!(BASHRC.contains(r#">>"$__giverny_hf_shared""#));
        assert!(!BASHRC.contains(r#" >"$__giverny_hf_shared""#));
    }

    /// Nothing the shell or a child of it reads as history is in the
    /// environment: the init files set it inside the shell.
    #[test]
    fn no_history_variable_is_exported() {
        for kind in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
            for shared in [false, true] {
                let s = steer_of(kind, shared, Some(("ZDOTDIR", "/u/zd")));
                for var in HISTORY_VARS {
                    assert_eq!(get(&s.env, var), None, "{kind:?} exports {var}");
                }
            }
        }
    }

    #[test]
    fn zsh_starts_on_the_init_dir_and_gets_its_zdotdir_back() {
        let s = steer_of(ShellKind::Zsh, true, None);
        assert!(s.args.is_empty());
        assert_eq!(get(&s.env, "ZDOTDIR"), Some("/s/shell/zsh"));
        assert_eq!(get(&s.env, "GIVERNY_HISTFILE"), Some("/s/history/3"));
        assert_eq!(get(&s.env, "GIVERNY_ZDOTDIR"), None, "none to put back");
        let s = steer_of(ShellKind::Zsh, false, Some(("ZDOTDIR", "/u/zd")));
        assert_eq!(get(&s.env, "GIVERNY_ZDOTDIR"), Some("/u/zd"));
    }

    #[test]
    fn fish_gets_a_session_name_it_accepts() {
        let s = steer_of(ShellKind::Fish, false, None);
        assert!(s.env.is_empty());
        assert_eq!(
            s.args,
            vec![
                "--init-command",
                "set -q fish_history; or set -g fish_history giverny_3"
            ]
        );
        assert!(
            fish_session(TabId(42))
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
    }

    #[test]
    fn init_files_are_written_once_and_kept_current() {
        let dir = scratch("init");
        let init = dir.join("init");
        assert_eq!(
            std::fs::read_to_string(init.join("bashrc")).unwrap(),
            BASHRC
        );
        assert_eq!(
            std::fs::read_to_string(init.join("zsh/.zshenv")).unwrap(),
            ZSHENV
        );
        std::fs::write(init.join("bashrc"), "stale").unwrap();
        write_init(&init).unwrap();
        assert_eq!(
            std::fs::read_to_string(init.join("bashrc")).unwrap(),
            BASHRC
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_takes_the_file() {
        let dir = std::env::temp_dir().join(format!("giverny-hist-{}", std::process::id()));
        let paths = Paths::at(&dir);
        let f = history_file(&paths, TabId(5));
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "ls\n").unwrap();
        remove(&paths, TabId(5));
        assert!(!f.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
