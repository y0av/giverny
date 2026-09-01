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
    "Stop",
    "Notification",
    "SessionEnd",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayMsg {
    /// `$GIVERNY_TAB_ID` as inherited by the hook (absent outside Giverny).
    pub tab_id: Option<String>,
    /// `$CLAUDE_CONFIG_DIR` — which account profile the session runs under.
    pub config_dir: Option<String>,
    /// The raw hook payload.
    pub event: serde_json::Value,
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

/// The `giverny relay` entrypoint. Fast, silent, always exits successfully —
/// a relay failure must never disturb the Claude session that ran the hook.
pub fn run_relay(spool: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1_000_000).read_to_string(&mut input);
    // Sessions outside Giverny tabs have no tab identity — nothing to relay
    // (and nothing worth spooling; the stdin read above keeps claude's
    // pipe-write happy before we bail).
    let Ok(tab_id) = std::env::var("GIVERNY_TAB_ID") else {
        return;
    };
    let event: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    let msg = RelayMsg {
        tab_id: Some(tab_id),
        config_dir: account_dir(),
        event,
    };
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
fn account_dir() -> Option<String> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .or_else(|| std::env::var("GIVERNY_PROFILE_DIR").ok())
        .filter(|dir| !dir.is_empty())
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
#[cfg(unix)]
pub fn spawn_listener(
    spool: &Path,
    wake: impl Fn() + Send + 'static,
) -> anyhow::Result<(crossbeam_channel::Receiver<RelayMsg>, Vec<RelayMsg>)> {
    use std::io::BufRead;
    use std::os::unix::net::{UnixListener, UnixStream};

    let spooled = drain_spool(spool);

    let path = socket_path();
    // A socket that still accepts connections belongs to a live instance —
    // unlinking and rebinding would silently steal its hook stream. Only a
    // stale socket (owner gone) may be replaced.
    if UnixStream::connect(&path).is_ok() {
        anyhow::bail!(
            "another Giverny is listening on {} — this window will not receive hook events",
            path.display()
        );
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let (tx, rx) = crossbeam_channel::unbounded();

    std::thread::Builder::new()
        .name("giverny hook listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tx = tx.clone();
                let reader = std::io::BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Ok(msg) = serde_json::from_str::<RelayMsg>(&line) {
                        let _ = tx.send(msg);
                        wake();
                    }
                }
            }
        })?;

    Ok((rx, spooled))
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
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "giverny".into())
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
/// Windows paths under `/mnt/c` land in `Program Files` often enough that
/// this is not hypothetical.
#[cfg(windows)]
fn shell_quote(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '~'))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Synthetic event name for statusline pushes (not a Claude hook event).
pub const STATUSLINE_EVENT: &str = "GivernyStatusLine";

/// The `giverny statusline` entrypoint: Claude Code runs this after every
/// assistant message and displays our stdout. We forward the payload's
/// official `rate_limits` to the app (fresh usage without any API call) and
/// print a compact line back.
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
    let msg = RelayMsg {
        tab_id: std::env::var("GIVERNY_TAB_ID").ok(),
        config_dir: account_dir(),
        event: serde_json::Value::Object(event),
    };
    deliver(&msg, spool);

    // What Claude displays. Keep it short and useful.
    let pct = |key: &str| -> Option<i64> {
        payload
            .get("rate_limits")?
            .get(key)?
            .get("used_percentage")?
            .as_f64()
            .map(|v| v.round() as i64)
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(model) = payload
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(|d| d.as_str())
    {
        parts.push(model.to_string());
    }
    if let Some(p) = pct("five_hour") {
        parts.push(format!("5h {p}%"));
    }
    if let Some(p) = pct("seven_day") {
        parts.push(format!("wk {p}%"));
    }
    println!("{}", parts.join("  ·  "));
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
        .is_some_and(|c| c.contains("giverny") && c.trim_end().ends_with("statusline"))
}

/// Install/remove the Giverny statusline. Refuses to replace a statusline
/// the user configured themselves.
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
        .is_some_and(|c| !c.contains("giverny"));
    if existing_is_foreign {
        anyhow::bail!("a custom statusLine is already configured — leaving it alone");
    }
    if enable {
        obj.insert(
            "statusLine".into(),
            serde_json::json!({
                "type": "command",
                "command": statusline_command_for(settings_path),
                "padding": 0,
            }),
        );
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
    let statusline_stale = root
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("giverny") && c != want_statusline);
    hooks_stale || statusline_stale
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
pub fn uninstall_from(settings_path: &Path) -> anyhow::Result<()> {
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
    let tmp = settings_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, settings_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn statusline_install_and_respect_existing() {
        let path = scratch("statusline");
        set_statusline(&path, true).unwrap();
        assert!(statusline_installed_in(&path));
        set_statusline(&path, false).unwrap();
        assert!(!statusline_installed_in(&path));

        std::fs::write(
            &path,
            r#"{"statusLine":{"type":"command","command":"my-own-script.sh"}}"#,
        )
        .unwrap();
        assert!(
            set_statusline(&path, true).is_err(),
            "must not clobber a user statusline"
        );
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
}
