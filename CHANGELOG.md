# Changelog

## v0.1.0 — unreleased

First public release. Built and daily-driven against Claude Code 2.1.220 on Linux.

### Terminal
- GPU-rendered grid on `alacritty_terminal` with a glyph atlas and integer-pixel cell metrics.
- Kitty keyboard protocol (Shift+Enter works in Claude with no `/terminal-setup`), synchronized output, bracketed paste with escape sanitizing, SGR/legacy mouse reporting, focus reporting, OSC 52 (store capped, reads denied).
- Selection with auto-copy (drag, double-click word, triple-click line), wheel scrollback, cursor blink, font zoom.
- Scrollback search (`Ctrl+Shift+F`) and Ctrl+click on URLs and file paths, including `file.rs:42`.

### Workspace
- Vertical rail of categorized tabs: colors, collapse, inline rename, drag-to-reorder across categories, context menus.
- Per-tab working directory, git branch, and Claude account.
- Everything persists across restarts and reboots, including scrollback (colors intact); shells respawn lazily on first focus.
- Fuzzy tab palette (`Ctrl+Shift+P`).

### Claude Code integration
- Live per-tab state: working spinner, needs-you flag with desktop notification, done marker; `Ctrl+Shift+A` jumps to the next tab that wants you.
- Hook relay (`giverny relay`) installed non-destructively into each profile's `settings.json`, with a registry-based fallback that needs no setup.
- Conversations resume on restore (`claude --resume`, correct directory and account, guarded against double-resume); past sessions browsable per tab.
- Multi-account usage meters read from Claude Code's own local files, refreshed live by an optional statusline. No API calls, no credential access.
- `giverny doctor` diagnoses the whole integration.

### Known gaps
- macOS and Windows compile in CI but have not had a real platform pass.
- No split panes, SSH/remote tabs, or plugin system.
