//! giverny-core: tabs, categories, config, and persistent state.
//!
//! Owns the tab/category model, the TOML config (with hot-reload), the
//! versioned `state/tabs.json` store + scrollback snapshots (atomic writes,
//! crash-safe), single-instance locking, and restore orchestration.
