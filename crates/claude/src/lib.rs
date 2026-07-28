//! giverny-claude: Claude Code integration.
//!
//! Watches Claude Code's local state (`sessions/<pid>.json` registry,
//! `.claude.json` usage cache, `jobs/`), installs/relays hooks, discovers
//! account profiles (`CLAUDE_CONFIG_DIR` dirs), and reads transcripts for
//! session titles and the resume picker. Never reads credentials, never
//! calls the network.
