//! Update checking.
//!
//! This is the *only* network request Giverny ever makes: a GET to GitHub's
//! releases API to compare the latest tag against this build. It is
//! disclosed, disableable (`[update] check = false`, or `GIVERNY_NO_UPDATE`),
//! throttled to once a day, and sends nothing but a User-Agent. Claude's
//! APIs are never contacted and credentials are never read — that promise is
//! unconditional and separate from this.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The project's home, shown in settings → about.
pub const REPO_URL: &str = "https://github.com/y0av/giverny";
const RELEASES_API: &str = "https://api.github.com/repos/y0av/giverny/releases/latest";
const INSTALL_SH: &str = "https://github.com/y0av/giverny/releases/latest/download/install.sh";
const INSTALL_PS1: &str = "https://github.com/y0av/giverny/releases/latest/download/install.ps1";
/// One check per day is plenty for a terminal you leave open for weeks.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateState {
    /// Unix seconds of the last completed check.
    pub checked_at: u64,
    /// Latest version seen upstream, without the `v`.
    pub latest: String,
}

/// A newer release than this build, if one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub url: String,
}

fn state_path(base: &Path) -> PathBuf {
    base.join("state").join("update.json")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compare dotted versions numerically. `None` when either side is unparsable
/// (a malformed tag must never look like an upgrade).
pub fn is_newer(candidate: &str, current: &str) -> Option<bool> {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let core = v.trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        core.split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    let (a, b) = (parse(candidate)?, parse(current)?);
    if a.is_empty() || b.is_empty() {
        return None;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return Some(x > y);
        }
    }
    Some(false)
}

/// Has the throttle elapsed? Also the place where "checking is off" is
/// honored, so callers cannot accidentally skip the opt-out.
pub fn should_check(base: &Path, enabled: bool) -> bool {
    if !enabled || std::env::var_os("GIVERNY_NO_UPDATE").is_some() {
        return false;
    }
    let last = load_state(base).checked_at;
    now_secs().saturating_sub(last) >= CHECK_INTERVAL_SECS
}

pub fn load_state(base: &Path) -> UpdateState {
    std::fs::read(state_path(base))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(base: &Path, state: &UpdateState) {
    let path = state_path(base);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Ask GitHub for the latest release tag. Blocking; run it off the UI thread.
pub fn fetch_latest() -> anyhow::Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .user_agent(concat!("giverny/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    // Read as text and parse here: ureq's json feature drags in a cookie
    // store we have no use for.
    let text = agent
        .get(RELEASES_API)
        .call()?
        .body_mut()
        .read_to_string()?;
    let body: serde_json::Value = serde_json::from_str(&text)?;
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("release payload has no tag_name"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// Full check: throttle, fetch, record, and report anything newer.
pub fn check(base: &Path, enabled: bool) -> Option<Available> {
    if !should_check(base, enabled) {
        // Still surface a newer version remembered from an earlier check.
        let state = load_state(base);
        return available_from(&state.latest);
    }
    match fetch_latest() {
        Ok(latest) => {
            save_state(
                base,
                &UpdateState {
                    checked_at: now_secs(),
                    latest: latest.clone(),
                },
            );
            available_from(&latest)
        }
        Err(err) => {
            tracing::info!("update check skipped: {err}");
            None
        }
    }
}

fn available_from(latest: &str) -> Option<Available> {
    if latest.is_empty() || !is_newer(latest, CURRENT).unwrap_or(false) {
        return None;
    }
    Some(Available {
        version: latest.to_string(),
        url: format!("https://github.com/y0av/giverny/releases/tag/v{latest}"),
    })
}

/// The command the update button runs — in a visible terminal tab, so the
/// user watches exactly what touches their machine.
pub fn install_command() -> String {
    if cfg!(windows) {
        format!("irm {INSTALL_PS1} | iex")
    } else {
        format!("curl -fsSL {INSTALL_SH} | sh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert_eq!(is_newer("0.2.0", "0.1.0"), Some(true));
        assert_eq!(
            is_newer("v0.1.1", "0.1.0"),
            Some(true),
            "tags may carry a v"
        );
        assert_eq!(is_newer("0.1.0", "0.1.0"), Some(false));
        assert_eq!(is_newer("0.1.0", "0.2.0"), Some(false), "never downgrade");
        assert_eq!(is_newer("1.0", "0.9.9"), Some(true), "short forms compare");
        assert_eq!(
            is_newer("0.10.0", "0.9.0"),
            Some(true),
            "numeric, not lexical"
        );
        assert_eq!(
            is_newer("0.2.0-rc1", "0.1.0"),
            Some(true),
            "pre-release suffix ignored"
        );
        assert_eq!(
            is_newer("garbage", "0.1.0"),
            None,
            "unparsable is never an upgrade"
        );
    }

    #[test]
    fn opt_out_is_honored() {
        let dir = std::env::temp_dir().join(format!("giverny-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(should_check(&dir, true), "first run checks");
        assert!(!should_check(&dir, false), "disabled in config means never");

        // A completed check silences the next 24h.
        save_state(
            &dir,
            &UpdateState {
                checked_at: now_secs(),
                latest: CURRENT.into(),
            },
        );
        assert!(!should_check(&dir, true), "throttled after a check");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembered_newer_version_is_offered_without_refetching() {
        let dir = std::env::temp_dir().join(format!("giverny-upd2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        save_state(
            &dir,
            &UpdateState {
                checked_at: now_secs(),
                latest: "99.0.0".into(),
            },
        );
        let found = check(&dir, true).expect("cached newer version is surfaced");
        assert_eq!(found.version, "99.0.0");
        assert!(found.url.ends_with("v99.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
