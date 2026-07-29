//! Per-account rate-limit meters from Claude Code's own on-disk cache
//! (`.claude.json → cachedUsageUtilization`). Zero network, zero credential
//! reads: this file is written by Claude Code itself after its own API calls.
//!
//! Bind only to `limits[]` (the canonical bucket list) — the legacy scalar
//! keys beside it are placeholder-ridden and churn.

use std::path::Path;

use serde::Deserialize;

use crate::profiles;

#[derive(Debug, Clone, Deserialize)]
pub struct AccountUsage {
    #[serde(rename = "fetchedAtMs", default)]
    pub fetched_at_ms: u64,
    #[serde(default)]
    pub limits: Vec<LimitEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitEntry {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub model: Option<ScopeModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScopeModel {
    #[serde(default)]
    pub display_name: Option<String>,
}

impl LimitEntry {
    /// Short label for a bar: `5h`, `week`, or the scoped model name.
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "session" => "5h".into(),
            "weekly_all" => "week".into(),
            "weekly_scoped" => self
                .scope
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .and_then(|m| m.display_name.clone())
                .unwrap_or_else(|| "model".into()),
            other => other.into(),
        }
    }

    fn resets_at_ts(&self) -> Option<jiff::Timestamp> {
        self.resets_at.as_deref()?.parse().ok()
    }

    /// Server quirk (verified live): when a window lapses with no activity,
    /// the cache keeps the *last* window's percent with a past `resets_at`.
    /// Those render as 0%.
    pub fn rolled_over(&self, now: jiff::Timestamp) -> bool {
        self.resets_at_ts().is_some_and(|t| t < now)
    }

    pub fn effective_percent(&self, now: jiff::Timestamp) -> f64 {
        if self.rolled_over(now) {
            0.0
        } else {
            self.percent.clamp(0.0, 100.0)
        }
    }

    pub fn critical(&self) -> bool {
        self.severity.as_deref() == Some("critical")
    }

    /// `"2h14m"`-style countdown to the reset, when in the future.
    pub fn reset_countdown(&self, now: jiff::Timestamp) -> Option<String> {
        let at = self.resets_at_ts()?;
        if at <= now {
            return None;
        }
        let secs = at.as_second() - now.as_second();
        let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
        Some(if d > 0 {
            format!("{d}d{h}h")
        } else if h > 0 {
            format!("{h}h{m:02}m")
        } else {
            format!("{m}m")
        })
    }
}

#[derive(Deserialize)]
struct CacheFile {
    #[serde(rename = "cachedUsageUtilization")]
    cached: Option<CachedUtilization>,
}

#[derive(Deserialize)]
struct CachedUtilization {
    #[serde(rename = "fetchedAtMs", default)]
    fetched_at_ms: u64,
    utilization: Option<Utilization>,
}

#[derive(Deserialize)]
struct Utilization {
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

/// Read one profile's usage cache.
pub fn read(config_dir: &Path) -> Option<AccountUsage> {
    let path = profiles::identity_path(config_dir);
    let bytes = std::fs::read(path).ok()?;
    let parsed: CacheFile = serde_json::from_slice(&bytes).ok()?;
    let cached = parsed.cached?;
    let limits = cached.utilization.map(|u| u.limits).unwrap_or_default();
    Some(AccountUsage {
        fetched_at_ms: cached.fetched_at_ms,
        limits,
    })
}

/// Ask Claude Code to refresh its own usage cache for one account.
///
/// `claude -p /usage` performs the fetch Claude would do interactively and
/// rewrites `cachedUsageUtilization`, which we then read as usual. This is
/// why Giverny needs no credentials and makes no Anthropic request of its
/// own: the first-party client does it, we just ask.
///
/// Blocking (seconds); call from a background thread. Failure is normal —
/// a logged-out account, no `claude` on PATH — and is not worth surfacing.
pub fn refresh_via_cli(config_dir: &Path) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("claude")
        .arg("-p")
        .arg("/usage")
        .env("CLAUDE_CONFIG_DIR", config_dir)
        // Never let it inherit a tab's identity: this is not a tab session.
        .env_remove("GIVERNY_TAB_ID")
        .env_remove("GIVERNY_NONCE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Bounded wait: a hung refresh must not leak a process forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                anyhow::bail!("usage refresh timed out");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// Cache age in minutes given the current wall clock.
pub fn age_minutes(usage: &AccountUsage, now: jiff::Timestamp) -> i64 {
    let now_ms = now.as_millisecond();
    ((now_ms - usage.fetched_at_ms as i64) / 60_000).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "oauthAccount": { "emailAddress": "x@y.z" },
      "cachedUsageUtilization": {
        "fetchedAtMs": 1785155896674,
        "accountUuid": "u-1",
        "utilization": {
          "five_hour": { "utilization": 4.0, "resets_at": "2026-07-28T17:09:59+00:00" },
          "tangelo": null,
          "limits": [
            { "kind": "session", "group": "session", "percent": 4, "severity": "normal",
              "resets_at": "2026-07-28T17:09:59+00:00", "scope": null, "is_active": false },
            { "kind": "weekly_all", "group": "weekly", "percent": 15, "severity": "normal",
              "resets_at": "2026-08-02T11:59:59+00:00", "scope": null, "is_active": false },
            { "kind": "weekly_scoped", "group": "weekly", "percent": 97, "severity": "critical",
              "resets_at": "2026-08-02T11:59:59+00:00",
              "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null },
              "is_active": true }
          ]
        }
      }
    }"#;

    fn fixture_usage() -> AccountUsage {
        let parsed: CacheFile = serde_json::from_str(FIXTURE).unwrap();
        let cached = parsed.cached.unwrap();
        AccountUsage {
            fetched_at_ms: cached.fetched_at_ms,
            limits: cached.utilization.unwrap().limits,
        }
    }

    #[test]
    fn parses_limits_with_scoped_model() {
        let u = fixture_usage();
        assert_eq!(u.limits.len(), 3);
        assert_eq!(u.limits[0].label(), "5h");
        assert_eq!(u.limits[1].label(), "week");
        assert_eq!(u.limits[2].label(), "Fable");
        assert!(u.limits[2].critical());
        assert!(u.limits[2].is_active);
    }

    #[test]
    fn rolled_over_windows_render_zero() {
        let u = fixture_usage();
        // "now" after the 5h reset but before the weekly reset.
        let now: jiff::Timestamp = "2026-07-28T19:00:00+00:00".parse().unwrap();
        assert_eq!(
            u.limits[0].effective_percent(now),
            0.0,
            "lapsed 5h window → 0%"
        );
        assert_eq!(u.limits[1].effective_percent(now), 15.0);
        let cd = u.limits[1].reset_countdown(now).unwrap();
        assert!(cd.ends_with('h'), "countdown in days+hours: {cd}");
    }

    #[test]
    fn tolerates_missing_cache() {
        let parsed: CacheFile = serde_json::from_str(r#"{"oauthAccount":{}}"#).unwrap();
        assert!(parsed.cached.is_none());
    }
}
