//! giverny-claude: Claude Code integration.
//!
//! Watches Claude Code's local state (`sessions/<pid>.json` registry,
//! `.claude.json` usage cache), installs/relays hooks, discovers account
//! profiles (`CLAUDE_CONFIG_DIR` dirs), and reads transcripts for session
//! titles; and measures what Giverny's tabs use (CPU, memory, GPU) from
//! `/proc`. Never reads credentials, never calls the network.

pub mod hooks;
pub mod jobs;
pub mod profiles;
pub mod registry;
pub mod session_use;
pub mod tokens;
pub mod usage;
pub mod use_reading;
pub mod wsl;
