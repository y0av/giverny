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
///
/// Claude Code puts it *beside* a default `~/.claude` and *inside* a
/// `CLAUDE_CONFIG_DIR`. Which layout applies is a property of the directory
/// rather than of whose home it is — an account in a WSL distribution, named
/// from Windows as `\\wsl.localhost\Ubuntu\home\x\.claude`, is the default
/// layout in that distribution and no `home_dir()` comparison here will ever
/// say so. A directory named `.claude` is the default layout, wherever it is.
///
/// The order matters, not just the existence: a home that once ran with
/// `CLAUDE_CONFIG_DIR` pointed at its own `~/.claude` has *both* files, the
/// inner one months stale. Preferring it would show usage from whenever that
/// experiment ended.
pub fn identity_path(config_dir: &Path) -> PathBuf {
    let inside = config_dir.join(".claude.json");
    let beside = config_dir.with_extension("json");
    let default_layout = config_dir.file_name().is_some_and(|n| n == ".claude");
    let (first, second) = if default_layout {
        (beside, inside)
    } else {
        (inside, beside)
    };
    if !first.is_file() && second.is_file() {
        return second;
    }
    first
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

/// Does this directory look like a Claude account, rather than something
/// that merely sits at a plausible path?
///
/// Claude Code writes an identity file and a session registry; requiring one
/// of them keeps an empty `~/.claude-old` out of the account list.
pub fn looks_like_account(dir: &Path) -> bool {
    dir.is_dir() && (identity_path(dir).is_file() || dir.join("sessions").is_dir())
}

/// Directories that could plausibly hold an account, without walking $HOME.
///
/// Deliberately shallow: `~/.claude`, anything named like it beside it, and
/// the XDG config location. Anywhere else, an account has to be named — by
/// `CLAUDE_CONFIG_DIR`, or in `behavior.extra_profile_dirs`.
fn scan_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let mut roots = vec![home.clone()];
    if let Some(config) = dirs::config_dir() {
        roots.push(config);
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `.claude`, `.claude-work`, `claude`, `claude-personal`…
            if name.trim_start_matches('.').starts_with("claude") {
                out.push(entry.path());
            }
        }
    }
    out
}

/// Directories Giverny finds without being told: the default account and
/// anything the shallow scan turns up.
///
/// Separate from `discover` because "would this be found anyway?" has to be
/// answerable *without* the environment — the environment is exactly what is
/// being decided about.
pub fn ambient_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let default = home.join(".claude");
        if default.is_dir() {
            out.push(default);
        }
    }
    out.extend(
        scan_candidates()
            .into_iter()
            .filter(|d| looks_like_account(d)),
    );
    // Accounts inside WSL: a Windows home directory cannot see them, and on
    // most Windows machines they are the only place Claude Code runs.
    out.extend(
        crate::wsl::account_dirs()
            .into_iter()
            .filter(|d| looks_like_account(d)),
    );
    out
}

/// Discover accounts, in priority order:
///
/// 1. `~/.claude` — Claude Code's default.
/// 2. `$CLAUDE_CONFIG_DIR` — Claude Code's own way of naming a config dir.
/// 3. A shallow scan of `~` and `~/.config` for `claude*` directories.
/// 4. `$CCTOP_CONFIG_DIRS` — a colon-separated list, supported for people
///    who already keep one; nothing here depends on it.
/// 5. `extra`, from `behavior.extra_profile_dirs` — the general answer for
///    accounts kept anywhere else.
///
/// Deduped by canonical path, order preserved. Candidates that do not look
/// like accounts are dropped, except ones named explicitly (2 and 5): if you
/// named it, you get told about it rather than silently ignored.
pub fn discover(extra: &[PathBuf]) -> Vec<Profile> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut push = |dir: PathBuf, require_evidence: bool| {
        if !dir.is_dir() || (require_evidence && !looks_like_account(&dir)) {
            return;
        }
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            out.push(profile_for(dir));
        }
    };

    if let Some(home) = dirs::home_dir() {
        push(home.join(".claude"), false);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        push(PathBuf::from(dir), false);
    }
    for dir in ambient_dirs() {
        push(dir, true);
    }
    if let Ok(list) = std::env::var("CCTOP_CONFIG_DIRS") {
        for part in list.split(':').filter(|p| !p.is_empty()) {
            push(PathBuf::from(part), true);
        }
    }
    for dir in extra {
        push(dir.clone(), false);
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-prof-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A directory that looks like a logged-in account.
    fn account(dir: &Path, email: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            format!(r#"{{"oauthAccount":{{"emailAddress":"{email}","accountUuid":"u"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn an_account_needs_evidence_not_just_a_plausible_name() {
        let root = scratch("evidence");
        let empty = root.join(".claude-old");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            !looks_like_account(&empty),
            "an empty dir is not an account"
        );

        let real = root.join(".claude-work");
        account(&real, "a@b.c");
        assert!(looks_like_account(&real));

        // A session registry counts too: a config dir used but never logged in.
        let fresh = root.join(".claude-fresh");
        std::fs::create_dir_all(fresh.join("sessions")).unwrap();
        assert!(looks_like_account(&fresh));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicitly_named_dirs_are_kept_even_without_evidence() {
        // If you listed it, an empty or not-yet-used dir should still appear,
        // so it can be seen to be empty rather than silently dropped.
        let root = scratch("named");
        let named = root.join("somewhere/odd");
        std::fs::create_dir_all(&named).unwrap();
        let found = discover(std::slice::from_ref(&named));
        assert!(
            found.iter().any(|p| p.config_dir == named),
            "a named dir was dropped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_is_named_after_its_account_not_its_directory() {
        let root = scratch("naming");
        let dir = root.join("work-claude");
        account(&dir, "sam@example.com");
        let p = profile_for(dir);
        assert_eq!(p.name, "sam");
        assert_eq!(p.email.as_deref(), Some("sam@example.com"));
    }

    #[test]
    fn identity_path_default_is_sibling() {
        let Some(home) = dirs::home_dir() else { return };
        let default = home.join(".claude");
        assert_eq!(identity_path(&default), home.join(".claude.json"));
        let custom = home.join("envs/x/claude");
        assert_eq!(identity_path(&custom), custom.join(".claude.json"));
    }

    /// A `.claude` anywhere is the default layout — which is how an account
    /// inside WSL, addressed from Windows, is found at all.
    #[test]
    fn a_dot_claude_anywhere_keeps_its_identity_beside_it() {
        let root = scratch("layouts");
        let dir = root.join("home").join("itay").join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let beside = dir.with_extension("json");
        std::fs::write(
            &beside,
            r#"{"oauthAccount":{"emailAddress":"itay@example.com","accountUuid":"u-9"}}"#,
        )
        .unwrap();
        assert_eq!(identity_path(&dir), beside);
        assert_eq!(profile_for(dir.clone()).name, "itay");

        // Both present: the sibling still wins, because the inner file is
        // the leftover of a CLAUDE_CONFIG_DIR that pointed here once.
        std::fs::write(dir.join(".claude.json"), r#"{"oauthAccount":{}}"#).unwrap();
        assert_eq!(identity_path(&dir), beside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A named config dir keeps it inside, even when something happens to
    /// sit beside it.
    #[test]
    fn a_named_config_dir_keeps_its_identity_inside() {
        let root = scratch("inside");
        let dir = root.join("work");
        account(&dir, "sam@example.com");
        std::fs::write(root.join("work.json"), r#"{"oauthAccount":{}}"#).unwrap();
        assert_eq!(identity_path(&dir), dir.join(".claude.json"));
        let _ = std::fs::remove_dir_all(&root);
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
