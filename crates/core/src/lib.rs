//! giverny-core: tabs, categories, config, and persistent state.
//!
//! Owns the tab/category model, the TOML config (with hot-reload), the
//! versioned `state/tabs.json` store + scrollback snapshots (atomic writes,
//! crash-safe), single-instance locking, and restore orchestration.

pub mod git;
pub mod state;
pub mod tabs;

/// Shorten a path for display: `~` for home, middle segments elided to fit.
pub fn short_path(path: &std::path::Path, max_chars: usize) -> String {
    let mut s = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home_s = home.display().to_string();
        if let Some(rest) = s.strip_prefix(&home_s) {
            s = format!("~{rest}");
        }
    }
    if s.chars().count() <= max_chars {
        return s;
    }
    // Keep the tail; elide the middle.
    let tail: String = s
        .chars()
        .rev()
        .take(max_chars.saturating_sub(2))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{}", tail.trim_start_matches(['/', '\\']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn short_path_elides_middle() {
        let p = Path::new("/very/long/path/to/some/deep/project/dir");
        let s = short_path(p, 16);
        assert!(s.chars().count() <= 17, "got {s:?}");
        assert!(s.ends_with("dir"));
        assert!(s.starts_with('…'));
    }

    #[test]
    fn short_path_keeps_short() {
        assert_eq!(short_path(Path::new("/tmp/x"), 20), "/tmp/x");
    }
}
