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
        config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok(),
        event,
    };
    let Ok(line) = serde_json::to_string(&msg) else {
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
    // App not running: spool so session-id captures survive (drained at the
    // app's next launch).
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

/// Bind the app-side listener. `wake` is called after each delivered message
/// (the app passes a repaint trigger). Returns the receiver plus any messages
/// spooled while the app was closed.
#[cfg(unix)]
pub fn spawn_listener(
    spool: &Path,
    wake: impl Fn() + Send + 'static,
) -> anyhow::Result<(crossbeam_channel::Receiver<RelayMsg>, Vec<RelayMsg>)> {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    // Drain the spool first.
    let mut spooled = Vec::new();
    if let Ok(content) = std::fs::read_to_string(spool) {
        for line in content.lines() {
            if let Ok(msg) = serde_json::from_str::<RelayMsg>(line) {
                spooled.push(msg);
            }
        }
        let _ = std::fs::remove_file(spool);
    }

    let path = socket_path();
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

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "giverny".into())
}

/// Synthetic event name for statusline pushes (not a Claude hook event).
pub const STATUSLINE_EVENT: &str = "GivernyStatusLine";

/// The `giverny statusline` entrypoint: Claude Code runs this after every
/// assistant message and displays our stdout. We forward the payload's
/// official `rate_limits` to the app (fresh usage without any API call) and
/// print a compact line back.
pub fn run_statusline() {
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
        config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok(),
        event: serde_json::Value::Object(event),
    };
    #[cfg(unix)]
    if let Ok(line) = serde_json::to_string(&msg) {
        use std::os::unix::net::UnixStream;
        if let Ok(mut stream) = UnixStream::connect(socket_path()) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = writeln!(stream, "{line}");
        }
    }

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
                "command": format!("{} statusline", exe_path()),
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

    let command = relay_command();
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
