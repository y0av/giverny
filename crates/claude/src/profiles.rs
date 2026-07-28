//! Claude account profiles: named `CLAUDE_CONFIG_DIR`s and their identities.
//!
//! Identity comes from `.claude.json` — which lives *inside* a custom config
//! dir but *beside* the default `~/.claude` (i.e. `~/.claude.json`). Accounts
//! are keyed by `oauthAccount.accountUuid`, never by directory name (the
//! same account can live in oddly-named dirs).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Profile {
    /// Short display name (email local part, else dir name).
    pub name: String,
    pub config_dir: PathBuf,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeJson {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<OauthAccount>,
}

#[derive(Deserialize)]
struct OauthAccount {
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
}

/// Where a config dir keeps its identity file.
pub fn identity_path(config_dir: &Path) -> PathBuf {
    let is_default = dirs::home_dir().is_some_and(|h| h.join(".claude") == config_dir);
    if is_default {
        // Sibling: ~/.claude.json
        config_dir.with_extension("json")
    } else {
        config_dir.join(".claude.json")
    }
}

fn read_identity(config_dir: &Path) -> (Option<String>, Option<String>) {
    let path = identity_path(config_dir);
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    let Ok(parsed) = serde_json::from_slice::<ClaudeJson>(&bytes) else {
        return (None, None);
    };
    match parsed.oauth_account {
        Some(acc) => (acc.email_address, acc.account_uuid),
        None => (None, None),
    }
}

fn profile_for(config_dir: PathBuf) -> Profile {
    let (email, account_uuid) = read_identity(&config_dir);
    let name = email
        .as_deref()
        .and_then(|e| e.split('@').next())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            config_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "claude".into())
        });
    Profile {
        name,
        config_dir,
        email,
        account_uuid,
    }
}

/// Discover profiles: the default `~/.claude`, plus any dirs in the
/// `CCTOP_CONFIG_DIRS` (colon-separated) convention, plus `extra` from
/// config. Deduped by canonical path, order preserved.
pub fn discover(extra: &[PathBuf]) -> Vec<Profile> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut push = |dir: PathBuf| {
        if !dir.is_dir() {
            return;
        }
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            out.push(profile_for(dir));
        }
    };

    if let Some(home) = dirs::home_dir() {
        push(home.join(".claude"));
    }
    if let Ok(list) = std::env::var("CCTOP_CONFIG_DIRS") {
        for part in list.split(':').filter(|p| !p.is_empty()) {
            push(PathBuf::from(part));
        }
    }
    for dir in extra {
        push(dir.clone());
    }
    out
}

/// The profile owning `config_dir`, if any.
pub fn find<'a>(profiles: &'a [Profile], config_dir: &Path) -> Option<&'a Profile> {
    profiles.iter().find(|p| p.config_dir == config_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_path_default_is_sibling() {
        let Some(home) = dirs::home_dir() else { return };
        let default = home.join(".claude");
        assert_eq!(identity_path(&default), home.join(".claude.json"));
        let custom = home.join("envs/x/claude");
        assert_eq!(identity_path(&custom), custom.join(".claude.json"));
    }

    #[test]
    fn profile_name_from_email() {
        let dir = std::env::temp_dir().join(format!("giverny-prof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"yoav@example.com","accountUuid":"u-1"}}"#,
        )
        .unwrap();
        let p = profile_for(dir.clone());
        assert_eq!(p.name, "yoav");
        assert_eq!(p.account_uuid.as_deref(), Some("u-1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
