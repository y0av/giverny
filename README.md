# Giverny

**A native GPU terminal built around Claude Code** — categorized persistent tabs on a left rail, live Claude activity on every tab, and usage meters for all your Claude accounts. *Where your Claudes live.*

Named for [Giverny](https://en.wikipedia.org/wiki/Giverny), Claude Monet's garden village — a home for the other famous Claude.

> Status: early development (M1 — terminal core). Not yet usable.

## Why

Running many concurrent Claude Code sessions across projects (and across multiple subscription accounts) in ordinary terminals means losing track of which session is working, which is waiting on you, and how close each account is to its rate limits. Giverny is a real terminal emulator — Rust, GPU-rendered, no web engine — whose entire chrome is built for that workflow:

- **Vertical, categorized tabs** that persist across restarts and reboots, restoring each shell's directory, scrollback, and — via `claude --resume` — the conversation itself.
- **Live Claude state per tab**: working spinner, needs-attention badge, done marker; driven by Claude Code hooks with zero-config fallbacks.
- **Multi-account aware**: per-tab/category account profiles (`CLAUDE_CONFIG_DIR`), and a usage panel showing every account's 5-hour / weekly / per-model limit bars — read entirely from Claude Code's own local state files. Giverny never touches your credentials and never calls any API.

## License

MIT OR Apache-2.0 (at your option).
