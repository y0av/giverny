//! The `giverny` Claude Code plugin: the manager the management panel needs,
//! carried inside the binary.
//!
//! The pane shows Running, Next up with ETAs and Done when a managing
//! session writes a feed; `giverny manage` writes it, and this plugin's
//! `manage` skill (`/giverny:manage`) tells Claude how to run a manager session
//! with it. Nothing outside Giverny is needed: no separate skill, no GitHub.
//!
//! How it reaches Claude Code, measured on 2.1.283:
//! - The plugin lives in a local **directory marketplace** under Giverny's
//!   own config dir. Two keys in an account's `settings.json` —
//!   `extraKnownMarketplaces.giverny` (a `directory` source) and
//!   `enabledPlugins["giverny@giverny"]` — are all Claude Code needs; no
//!   `claude plugin install`, no network.
//! - A directory source is loaded straight from the directory at session
//!   start, not from a cache, so the files this binary writes are what the
//!   next session runs. The plugin's version is the binary's.
//! - Its `bin/` is on the Bash tool's `PATH`, so its launchers work in any
//!   session and in its subagents without Giverny on `PATH`:
//!   `giverny-manage` (the manage skill's), `giverny-eta`
//!   (any subagent's ETA) and `giverny-hook` (the hook's).
//! - It coexists with a project's own `/manage` skill: plugin skills are
//!   namespaced. Its one command, `/giverny:clear-done`, runs
//!   `giverny-manage clear-done` to clear the management panel's Done rows.
//! - Removing the keys unloads it; a missing directory makes Claude Code skip
//!   it silently.
//!
//! - `hooks/hooks.json` carries a `PostToolUse` hook running `giverny-hook`
//!   ([`crate::plugin_hook`]): a dispatcher that just started a worker with
//!   no estimate is asked for one, and five minutes into its work the worker
//!   is asked, once, to correct it, so every subagent gets an ETA. Any other
//!   call costs a few `stat`s. The same launcher runs as a `PreToolUse` hook
//!   on Bash: a command that would leave its tab's resource cap
//!   (`systemd-run`, `systemctl --user set-property`, a write into
//!   `/sys/fs/cgroup`, …) runs with a reminder, once per kind a session
//!   ([`crate::bash_guard`]), and one
//!   that reaches into another session's processes, run scopes or
//!   worktrees, and on Edit/Write an edit in such a worktree, is refused
//!   ([`crate::reach_guard`]). Before a subagent's
//!   `SubagentHandback` it holds a task worker's report back once while
//!   background shells of its own still run ([`crate::plugin_hook::handback`]).
//!   And with the
//!   skill in, it runs as a `UserPromptSubmit` hook: a prompt that asks to
//!   manage ("manage that task") gets context telling Claude to invoke
//!   `giverny:manage` first, since a skill picked only by its description
//!   loses to "manage" read as a plain verb ([`crate::manage_trigger`]).
//!   With the skill in it is a `PreToolUse` hook on the `Agent` tool too: a
//!   session that is no manager, starting a subagent that looks like task
//!   work, is told once that the skill is there ([`crate::manage_hint`]).
//!
//! - It runs as a `SessionStart` hook as well: with
//!   `management_panel.ask_names` on (off by default), a
//!   session in a Giverny tab is named right there, before its first turn:
//!   the hook draws a name, records it and tells the session
//!   ([`crate::names`]). The hook is there with
//!   the switch off too, and reads `<feeds>/names/on` itself: Claude Code
//!   reads `hooks.json` only when a claude starts (or on `/reload-plugins`),
//!   so a hook added when the switch goes on would never reach the sessions
//!   already running, not even at their `/clear` (measured on
//!   2.1.296).
//!
//! - And as a `SubagentStop` hook: a subagent finishing with a background
//!   shell or monitor of its own still running is asked, once, whether it
//!   should keep running after it reports, and its parent session told of
//!   what it kept ([`crate::leftovers`]). By the same rule it
//!   reaches only sessions started after the update that wrote it.
//! - And as a `Stop` hook: a session's turn ending with a background shell
//!   of its own that its timeout moved there, or that has gone quiet, is
//!   asked about once per command ([`crate::strays`]).
//!
//! The skill is its own switch (`management_panel.manage_skill`, on by
//! default): off, the plugin is written without `skills/` and without its
//! prompt hook, so the tool hooks, the launchers and `/giverny:clear-done`
//! the pane needs stay, and only the skill goes — from every account at once, since they all load this
//! one directory.
//!
//! - A plugin cannot grant permissions: of a plugin's own `settings.json`
//!   Claude Code keeps only `agent` and `subagentStatusLine`, and a skill's
//!   `allowed-tools` lasts one turn of the session that ran the skill — never
//!   a worker's. So the account's `settings.json` carries [`ALLOW_RULES`] in
//!   `permissions.allow` beside the two keys, or a worker's `eta`, `run` and
//!   `claim` each stop on a permission prompt.
//!
//! The settings keys follow the house rules the other management-panel key does:
//! written only with `claude.management_panel` on, never over a
//! marketplace called `giverny` that is not ours, removed when the setting
//! goes off and on uninstall, and a no-op writes nothing.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const MARKETPLACE: &str = "giverny";
pub const PLUGIN: &str = "giverny";
pub const PLUGIN_ID: &str = "giverny@giverny";
/// The Bash permission rules that let every session run the plugin's
/// launchers, and the subcommand they wrap, without asking.
///
/// Claude Code splits a command at `&&`, `;` and `|` and matches each part on
/// its own, and its read-only commands (`cd` within the project, `tail`,
/// `grep`, …) need no rule; so `cd <worktree> && giverny-manage run …` and
/// `giverny-manage eta … 2>&1 | tail -3` pass on the first rule alone.
/// `giverny-hook` runs as a hook, not through Bash, and needs none.
pub const ALLOW_RULES: &[&str] = &[
    "Bash(giverny-manage:*)",
    "Bash(giverny-eta:*)",
    "Bash(giverny manage:*)",
];

/// The marketplace directory's name under Giverny's config base.
pub const DIR_NAME: &str = "claude-plugin";
/// The plugin's version: the binary's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const SKILL: &str = include_str!("../plugin/skills/manage/SKILL.md");
/// `/giverny:clear-done`: the management panel's Done rows, cleared.
const CLEAR_DONE: &str = include_str!("../plugin/commands/clear-done.md");

/// Where the marketplace lives: `<giverny config base>/claude-plugin`.
pub fn marketplace_dir(base: &Path) -> PathBuf {
    base.join(DIR_NAME)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A `bin/` launcher: runs `<the binary that wrote it> <sub>`.
fn wrapper(exes: &[String], sub: &str) -> String {
    let list: Vec<String> = exes.iter().map(|e| sh_quote(e)).collect();
    format!(
        "#!/bin/sh\n\
         # Written by Giverny {VERSION}; rewritten each time it starts with the\n\
         # management panel on. Runs `giverny {sub}`: see `giverny {sub} --help`.\n\
         for g in {list}; do\n  \
           if [ -x \"$g\" ]; then exec \"$g\" {sub} \"$@\"; fi\n\
         done\n\
         if command -v giverny >/dev/null 2>&1; then exec giverny {sub} \"$@\"; fi\n\
         echo \"$0: the Giverny that wrote it is gone\" >&2\n\
         exit 127\n",
        list = list.join(" ")
    )
}

/// `hooks/hooks.json`: a `PostToolUse` hook, `giverny-hook`, which asks a
/// dispatcher for the estimate of a worker it just started, and a worker
/// five minutes in to correct it; on a manager's own calls it delivers
/// the session's `ask`/`reply` messages and renews its resource leases;
/// quiet and exit 0 whatever happens. The same launcher is a `PreToolUse`
/// hook on Bash, which refuses a command that would leave its tab's
/// resource cap ([`crate::bash_guard`]) — and on Bash and the editing tools
/// one that would reach into another session's processes, scopes or
/// worktrees ([`crate::reach_guard`]) — and, with the manage skill in
/// (`skill`), a `UserPromptSubmit` hook that sends a prompt asking to manage
/// to that skill ([`crate::manage_trigger`]) and a `PreToolUse` hook on the
/// `Agent` tool (`Task` in older Claude Code) that nudges a session toward
/// the skill when a subagent it starts looks like task work
/// ([`crate::manage_hint`]). With the skill off both go: they
/// would point at a skill that is not there.
///
/// The same launcher is a `SessionStart` hook too, on every source (startup,
/// resume, clear, compact): with `management_panel.ask_names` on,
/// a Giverny tab's session is given a name and told it, or
/// told the name it has ([`crate::names`]). It is registered whatever the switch
/// says, which the hook reads as it runs: Claude Code loads `hooks.json` once
/// per process, so a hook written only with the switch on misses every
/// session started before it went on. Off, it costs a `stat`.
///
/// And it is a `SubagentStop` hook: a subagent finishing with a background
/// command of its own still running is asked, once, whether it should keep
/// running, and its parent told if it does ([`crate::leftovers`]).
/// And a `Stop` hook: a session's turn ending with a
/// background shell of its own that its timeout moved there, or that has
/// gone quiet, is held once per command with a question
/// ([`crate::strays`]). The `Stop` hook and a `SessionEnd`
/// hook also keep a named session's `/resume` row its name and its text
/// ([`crate::names::keep_title`]). Like any hook here it reaches only
/// sessions started after the `hooks.json` that names it was written.
/// The tools the `PreToolUse` guard sees: Bash, for the cap
/// ([`crate::bash_guard`]) and for reaching into another session's work,
/// and the editing tools, for its worktrees ([`crate::reach_guard`]),
/// and a subagent's report tool, held back once while a task
/// worker's own background shells still run
/// ([`crate::plugin_hook::handback`]).
pub const PRE_MATCHER: &str = "Bash|Edit|Write|MultiEdit|NotebookEdit|SubagentHandback";

fn session_hooks(skill: bool) -> Value {
    let hook = || {
        json!([{
            "type": "command",
            "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/giverny-hook\" 2>/dev/null || true"
        }])
    };
    let mut hooks = json!({
        "PreToolUse": [{ "matcher": PRE_MATCHER, "hooks": hook() }],
        "PostToolUse": [{ "matcher": "*", "hooks": hook() }],
        "SessionStart": [{ "hooks": hook() }],
        "SubagentStop": [{ "hooks": hook() }],
        "Stop": [{ "hooks": hook() }],
        "SessionEnd": [{ "hooks": hook() }]
    });
    if skill {
        hooks["UserPromptSubmit"] = json!([{ "hooks": hook() }]);
        hooks["PreToolUse"] = json!([
            { "matcher": PRE_MATCHER, "hooks": hook() },
            { "matcher": "Agent|Task", "hooks": hook() }
        ]);
    }
    json!({
        "description": "Giverny: ETAs for subagents, manager sessions' upkeep, \
                        Bash commands kept inside their tab's resource cap \
                        and out of other sessions' work, \
                        \"manage this\" sent to the manage skill, \
                        task work pointed at it, \
                        and sessions and subagents asked about commands they leave running",
        "hooks": hooks
    })
}

/// Every file of the marketplace: (path under the dir, contents, executable).
/// `skill` false leaves the manage skill out.
pub fn files(exes: &[String], skill: bool) -> Vec<(&'static str, String, bool)> {
    let marketplace = json!({
        "name": MARKETPLACE,
        "owner": { "name": "Giverny" },
        "description": "The Claude Code plugin that ships inside Giverny",
        "plugins": [{
            "name": PLUGIN,
            "source": "./plugins/giverny",
            "description": "Manage subagents and show them in Giverny's management panel",
            "version": VERSION
        }]
    });
    let plugin = json!({
        "name": PLUGIN,
        "version": VERSION,
        "description": "Manage subagents and show them in Giverny's management panel: \
                        Running, Next up with ETAs, Done",
        "author": { "name": "Giverny" }
    });
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    let mut out = vec![
        (
            ".claude-plugin/marketplace.json",
            pretty(&marketplace),
            false,
        ),
        (
            "plugins/giverny/.claude-plugin/plugin.json",
            pretty(&plugin),
            false,
        ),
        (
            "plugins/giverny/commands/clear-done.md",
            CLEAR_DONE.to_string(),
            false,
        ),
        (
            "plugins/giverny/bin/giverny-manage",
            wrapper(exes, "manage"),
            true,
        ),
        (
            "plugins/giverny/bin/giverny-eta",
            wrapper(exes, "eta"),
            true,
        ),
        (
            "plugins/giverny/bin/giverny-hook",
            wrapper(exes, "hook"),
            true,
        ),
        (
            "plugins/giverny/hooks/hooks.json",
            pretty(&session_hooks(skill)),
            false,
        ),
    ];
    if skill {
        out.push((SKILL_PATH, SKILL.to_string(), false));
    }
    out
}

/// Whether this build installs the plugin at all. Its launchers
/// (`bin/giverny-manage`, `bin/giverny-eta`, `bin/giverny-hook`) are
/// `#!/bin/sh` scripts, which native-Windows Claude Code cannot run; there
/// the plugin is neither written nor named in any account, and what an older
/// build wrote is taken back as the switch's off does.
pub const SUPPORTED: bool = cfg!(unix);

/// Where the manage skill sits in the marketplace.
pub const SKILL_PATH: &str = "plugins/giverny/skills/manage/SKILL.md";

/// The binary as the wrapper should name it: this one, and on Windows also
/// its path from inside WSL (a WSL account's Claude runs it through interop).
pub fn exe_candidates() -> Vec<String> {
    let mut out = Vec::new();
    // The stable link when there is one ([`crate::hooks::link_path`]): the
    // wrapper then stays byte-identical across rebuilds and reinstalls.
    #[cfg(unix)]
    if let Some(link) = crate::hooks::link_path()
        && link.exists()
    {
        return vec![link.display().to_string()];
    }
    if let Some(exe) = crate::hooks::running_exe() {
        out.push(exe.display().to_string());
        #[cfg(windows)]
        if let Some(distro) = crate::wsl::default_distro()
            && let Some(inside) = crate::wsl::to_wsl_path(&distro, &exe)
        {
            out.push(inside);
        }
    }
    out
}

/// Write the marketplace into `dir`, touching only files whose bytes differ,
/// and removing anything else there (the directory is ours alone). Returns
/// whether anything changed. `skill` false writes the plugin without its
/// manage skill (and so prunes one written before).
pub fn sync(dir: &Path, exes: &[String], skill: bool) -> std::io::Result<bool> {
    let want = files(exes, skill);
    let mut changed = false;
    for (rel, body, exec) in &want {
        let path = dir.join(rel);
        if std::fs::read(&path).is_ok_and(|b| b == body.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("giverny-tmp");
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if *exec { 0o755 } else { 0o644 };
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = exec;
        std::fs::rename(&tmp, &path)?;
        changed = true;
    }
    let keep: Vec<PathBuf> = want.iter().map(|(rel, _, _)| dir.join(rel)).collect();
    changed |= prune(dir, &keep)?;
    Ok(changed)
}

/// Remove every file under `dir` not in `keep`, and directories left empty.
fn prune(dir: &Path, keep: &[PathBuf]) -> std::io::Result<bool> {
    let mut changed = false;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(false);
    };
    for e in entries.flatten() {
        let p = e.path();
        if e.file_type()?.is_dir() {
            changed |= prune(&p, keep)?;
            if std::fs::read_dir(&p)?.next().is_none() {
                std::fs::remove_dir(&p)?;
                changed = true;
            }
        } else if !keep.contains(&p) {
            std::fs::remove_file(&p)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Delete the marketplace directory — only when it is ours (its
/// `marketplace.json` names the `giverny` marketplace).
pub fn remove_dir(dir: &Path) -> std::io::Result<bool> {
    let manifest = dir.join(".claude-plugin/marketplace.json");
    let ours = std::fs::read(&manifest)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v.get("name").and_then(Value::as_str) == Some(MARKETPLACE));
    if !ours {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

/// The marketplace path as the account's Claude Code must read it: a WSL
/// account on Windows reads a Windows directory through `/mnt/<drive>`.
pub fn dir_for(settings_path: &Path, dir: &Path) -> String {
    #[cfg(windows)]
    if let Some((distro, _)) = crate::wsl::split_unc(settings_path)
        && let Some(inside) = crate::wsl::to_wsl_path(&distro, dir)
    {
        return inside;
    }
    let _ = settings_path;
    dir.display().to_string()
}

/// Is this `extraKnownMarketplaces` entry (or `known_marketplaces.json`
/// entry) Giverny's: a directory source ending in `giverny/claude-plugin`?
fn is_our_source(entry: &Value) -> bool {
    let src = entry.get("source");
    src.and_then(|s| s.get("source")).and_then(Value::as_str) == Some("directory")
        && src
            .and_then(|s| s.get("path"))
            .and_then(Value::as_str)
            .is_some_and(|p| {
                let p = p.trim_end_matches(['/', '\\']).replace('\\', "/");
                p.ends_with(&format!("giverny/{DIR_NAME}"))
            })
}

/// Is the plugin configured (ours, enabled) in this settings file?
pub fn installed_in(settings_path: &Path) -> bool {
    let Some(root) = std::fs::read(settings_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return false;
    };
    root.get("extraKnownMarketplaces")
        .and_then(|m| m.get(MARKETPLACE))
        .is_some_and(is_our_source)
        && root
            .get("enabledPlugins")
            .and_then(|m| m.get(PLUGIN_ID))
            .and_then(Value::as_bool)
            == Some(true)
}

/// Add or remove the plugin's two keys in one account's `settings.json`.
/// Returns whether the file changed.
///
/// On: `extraKnownMarketplaces.giverny` points at `dir` (refused when a
/// marketplace of that name is someone else's) and `enabledPlugins`
/// gains `giverny@giverny: true` unless the user already set it — a user who
/// ran `claude plugin disable` keeps it disabled. `permissions.allow` gains
/// whichever of [`ALLOW_RULES`] it lacks, at its end; the user's own rules
/// keep their order. Off: both are removed, only
/// when ours, a map we emptied goes with them, and Claude Code's own record
/// of the marketplace (`plugins/known_marketplaces.json`) loses its entry
/// too, as `claude plugin marketplace remove` would; so do our allow rules.
pub fn set_plugin(settings_path: &Path, dir: &Path, enable: bool) -> anyhow::Result<bool> {
    let changed = set_keys(settings_path, dir, enable)?;
    if !enable && let Some(config) = settings_path.parent() {
        forget_known(&config.join("plugins").join("known_marketplaces.json"));
    }
    Ok(changed)
}

fn set_keys(settings_path: &Path, dir: &Path, enable: bool) -> anyhow::Result<bool> {
    let mut root: Value = match std::fs::read(settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("won't touch unparseable settings: {e}"))?,
        Err(_) if !enable => return Ok(false),
        Err(_) => json!({}),
    };
    // What is there now: a file this would not change is not written.
    let before = root.clone();
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not an object"))?;
    let current = obj
        .get("extraKnownMarketplaces")
        .and_then(|m| m.get(MARKETPLACE))
        .cloned();
    if let Some(entry) = &current
        && !is_our_source(entry)
    {
        anyhow::bail!("a marketplace called `giverny` is already configured — leaving it alone");
    }
    let want = json!({ "source": { "source": "directory", "path": dir_for(settings_path, dir) } });
    set_allow_rules(obj, enable);
    if enable {
        let m = obj
            .entry("extraKnownMarketplaces")
            .or_insert_with(|| json!({}));
        m.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("extraKnownMarketplaces is not an object"))?
            .insert(MARKETPLACE.into(), want);
        let p = obj.entry("enabledPlugins").or_insert_with(|| json!({}));
        p.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("enabledPlugins is not an object"))?
            .entry(PLUGIN_ID)
            .or_insert(json!(true));
    } else {
        for (map, key) in [
            ("extraKnownMarketplaces", MARKETPLACE),
            ("enabledPlugins", PLUGIN_ID),
        ] {
            let emptied = obj
                .get_mut(map)
                .and_then(Value::as_object_mut)
                .is_some_and(|m| m.remove(key).is_some() && m.is_empty());
            if emptied {
                obj.remove(map);
            }
        }
    }
    if root == before && settings_path.exists() {
        return Ok(false);
    }
    let backup = settings_path.with_extension("json.giverny-bak");
    if settings_path.exists() && !backup.exists() {
        let _ = std::fs::copy(settings_path, &backup);
    }
    let tmp = settings_path.with_extension("json.tmp");
    if let Some(d) = settings_path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(true)
}

/// Append the [`ALLOW_RULES`] `permissions.allow` lacks, or take them out
/// again; every other rule stays where it is. A `permissions` or `allow`
/// that is not the shape Claude Code reads is the user's to fix, and is left
/// alone. Off, an `allow` or `permissions` that only we filled goes too.
fn set_allow_rules(obj: &mut serde_json::Map<String, Value>, enable: bool) {
    if enable {
        let Some(perms) = obj
            .entry("permissions")
            .or_insert_with(|| json!({}))
            .as_object_mut()
        else {
            return;
        };
        let Some(list) = perms
            .entry("allow")
            .or_insert_with(|| json!([]))
            .as_array_mut()
        else {
            return;
        };
        for rule in ALLOW_RULES {
            if !list.iter().any(|v| v.as_str() == Some(rule)) {
                list.push(json!(rule));
            }
        }
        return;
    }
    let Some(perms) = obj.get_mut("permissions").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(list) = perms.get_mut("allow").and_then(Value::as_array_mut) else {
        return;
    };
    let had = list.len();
    list.retain(|v| !v.as_str().is_some_and(|r| ALLOW_RULES.contains(&r)));
    if had == list.len() || !list.is_empty() {
        return;
    }
    perms.remove("allow");
    if perms.is_empty() {
        obj.remove("permissions");
    }
}

/// Drop our entry from Claude Code's `known_marketplaces.json`, which it
/// fills from `extraKnownMarketplaces` and does not empty by itself.
fn forget_known(path: &Path) {
    let Some(mut root) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return;
    };
    let Some(map) = root.as_object_mut() else {
        return;
    };
    if !map.get(MARKETPLACE).is_some_and(is_our_source) {
        return;
    }
    map.remove(MARKETPLACE);
    let tmp = path.with_extension("json.giverny-tmp");
    if serde_json::to_vec_pretty(&root)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .is_some()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Bring the manage skill in the plugin at `plugin_root` (`CLAUDE_PLUGIN_ROOT`,
/// `<marketplace>/plugins/giverny`) up to this binary's text, when the
/// marketplace is ours and the skill is in it. The app writes
/// the plugin only when it starts, while the launchers run the binary on
/// disk: rebuilt under a running app, the CLI is new and the skill telling
/// dispatchers and workers how to use it old. The hook calls this, so the
/// skill a session reads matches the `giverny-manage` it runs. Only the
/// skill's own file is touched, and only when its bytes differ; a skill
/// switched off (no file) stays off. True when it was rewritten.
pub fn refresh_skill(plugin_root: &Path) -> std::io::Result<bool> {
    let Some(market) = plugin_root.parent().and_then(Path::parent) else {
        return Ok(false);
    };
    let ours = std::fs::read(market.join(".claude-plugin/marketplace.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v.get("name").and_then(Value::as_str) == Some(MARKETPLACE));
    let path = market.join(SKILL_PATH);
    if !ours || path != plugin_root.join("skills/manage/SKILL.md") {
        return Ok(false);
    }
    match std::fs::read(&path) {
        Ok(b) if b != SKILL.as_bytes() => {}
        _ => return Ok(false),
    }
    let tmp = path.with_extension("giverny-tmp");
    std::fs::write(&tmp, SKILL)?;
    std::fs::rename(&tmp, &path)?;
    Ok(true)
}

/// [`refresh_skill`] for the plugin this hook runs from:
/// `$CLAUDE_PLUGIN_ROOT`, which Claude Code sets for a plugin's hooks, else
/// the plugin in Giverny's default config dir.
pub fn refresh_own_skill() {
    let root = std::env::var_os("CLAUDE_PLUGIN_ROOT")
        .filter(|r| !r.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let base = crate::feed::default_dir().parent()?.to_path_buf();
            Some(marketplace_dir(&base).join("plugins").join(PLUGIN))
        });
    if let Some(root) = root {
        let _ = refresh_skill(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-plugin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_on_off_round_trips_and_leaves_the_rest() {
        let d = scratch("settings");
        let s = d.join("settings.json");
        let mine = r#"{"enabledPlugins":{"other@x":true},"model":"opus"}"#;
        std::fs::write(&s, mine).unwrap();
        let dir = d.join("giverny").join(DIR_NAME);
        assert!(set_plugin(&s, &dir, true).unwrap());
        assert!(installed_in(&s));
        assert!(!set_plugin(&s, &dir, true).unwrap(), "no-op writes nothing");
        let v: Value = serde_json::from_slice(&std::fs::read(&s).unwrap()).unwrap();
        assert_eq!(v["enabledPlugins"]["other@x"], true);
        assert_eq!(v["enabledPlugins"][PLUGIN_ID], true);
        assert_eq!(
            v["extraKnownMarketplaces"]["giverny"]["source"]["path"],
            dir.display().to_string()
        );
        // Claude Code's own record of the marketplace.
        let known = d.join("plugins/known_marketplaces.json");
        std::fs::create_dir_all(known.parent().unwrap()).unwrap();
        std::fs::write(
            &known,
            serde_json::to_vec(&json!({
                "giverny": v["extraKnownMarketplaces"]["giverny"].clone(),
                "theirs": {"source": {"source": "github", "repo": "a/b"}}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(set_plugin(&s, &dir, false).unwrap());
        assert!(!installed_in(&s));
        let v: Value = serde_json::from_slice(&std::fs::read(&s).unwrap()).unwrap();
        let orig: Value = serde_json::from_str(mine).unwrap();
        assert_eq!(v, orig, "off leaves the file as it was, as JSON");
        let k: Value = serde_json::from_slice(&std::fs::read(&known).unwrap()).unwrap();
        assert!(k.get("giverny").is_none() && k.get("theirs").is_some());
        assert!(
            !set_plugin(&s, &dir, false).unwrap(),
            "off twice is a no-op"
        );
    }

    #[test]
    fn unreadable_settings_and_odd_permissions_are_left_alone() {
        let d = scratch("allow-odd");
        let s = d.join("settings.json");
        let dir = d.join("giverny").join(DIR_NAME);
        let broken = r#"{"permissions":{"allow":["Bash(ls:*)",]}"#;
        std::fs::write(&s, broken).unwrap();
        assert!(set_plugin(&s, &dir, true).is_err());
        assert!(set_plugin(&s, &dir, false).is_err());
        assert_eq!(std::fs::read_to_string(&s).unwrap(), broken);
        assert!(!s.with_extension("json.giverny-bak").exists());

        // Valid JSON, but not the shape Claude Code reads: the plugin still
        // loads, and the user's odd value is theirs to fix.
        for odd in [
            r#"{"permissions":"ask"}"#,
            r#"{"permissions":{"allow":"Bash(ls:*)"}}"#,
        ] {
            std::fs::write(&s, odd).unwrap();
            assert!(set_plugin(&s, &dir, true).unwrap());
            assert!(installed_in(&s));
            let v: Value = serde_json::from_slice(&std::fs::read(&s).unwrap()).unwrap();
            let orig: Value = serde_json::from_str(odd).unwrap();
            assert_eq!(v["permissions"], orig["permissions"]);
            assert!(!set_plugin(&s, &dir, true).unwrap(), "no-op writes nothing");
        }
    }

    #[test]
    fn someone_elses_giverny_marketplace_and_a_users_disable_are_kept() {
        let d = scratch("theirs");
        let s = d.join("settings.json");
        let dir = d.join("giverny").join(DIR_NAME);
        let theirs = r#"{"extraKnownMarketplaces":{"giverny":{"source":{"source":"github","repo":"x/giverny"}}}}"#;
        std::fs::write(&s, theirs).unwrap();
        assert!(set_plugin(&s, &dir, true).is_err());
        assert!(set_plugin(&s, &dir, false).is_err());
        assert_eq!(std::fs::read_to_string(&s).unwrap(), theirs);

        std::fs::write(
            &s,
            format!(r#"{{"enabledPlugins":{{"{PLUGIN_ID}":false}}}}"#),
        )
        .unwrap();
        assert!(set_plugin(&s, &dir, true).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&s).unwrap()).unwrap();
        assert_eq!(
            v["enabledPlugins"][PLUGIN_ID], false,
            "their disable stands"
        );
        assert!(!installed_in(&s));
    }
}
