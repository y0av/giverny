# Changelog

## v0.3.0 — 2026-07-29

### Settings
- A settings screen (`Ctrl+,`), drawn over the terminal with the rail still
  visible. Search across every option, per-row reset, and each row labelled
  with its TOML key.
- Options are declared once and generate the screen, the commented
  `config.toml` written on first run, and [docs/options.md](docs/options.md).
  Tests fail if the three disagree.
- Edits are written back with `toml_edit`: your comments and layout survive.
- `F1` lists every key binding, from the same table the settings screen uses.
- Titles: `titles.strip_host_prefix` (on by default) drops the `user@host:`
  your shell puts in front of every title; `titles.shorten_paths` abbreviates
  all but the last directory. Both apply at display time, so toggling either
  updates existing tabs at once.
- Four more themes: Tokyo Night, Gruvbox, Nord, Catppuccin.
- The theme now colours Giverny's own chrome too — rail, settings, overlays —
  taking its accents from the theme's own ANSI palette. Picking Gruvbox used
  to recolour the grid and leave a Monet-blue rail around it.
- Settings → about links to the repo, and a ⚙ next to the accounts refresh
  opens settings without knowing the chord.
- Options that only load at startup (font family, extra account directories)
  say so once you change them, instead of looking like a row that does
  nothing.

### Not yet
Live theme preview on hover, chord hints in the command palette, key
rebinding, and theme files loaded from `~/.config/giverny/themes/`.
- Font size now lives only in `config.toml`. `Ctrl` `+`/`−`/`0` writes it
  there; a size saved by an older build is migrated on first run.

## v0.2.0 — 2026-07-29

Everything since the first release: installers, an update check, and the fixes
found by actually living in it.

### Install & update
- One-line installers for Linux, macOS and Windows. Prebuilt binaries for
  x86_64 Linux, both Macs, and Windows; arm64 Linux builds from source until
  the next release adds it.
- Giverny checks GitHub once a day for a newer version and offers a one-click
  update; `giverny update` does it from the shell. Opt out with
  `[update] check = false` or `GIVERNY_NO_UPDATE=1`.
- `giverny install-desktop` installs the icon and `.desktop` entry (needed for
  a proper taskbar icon on Wayland).

### Restore
- Tabs restart the full-screen program they were running (btop, k9s, lazygit…).
  Anything outside `behavior.restore_apps` is remembered but never re-run —
  replaying an arbitrary last command could deploy or delete something.
- Window size, maximized state and rail width reopen where you left them.

### Claude integration
- Usage refreshes on its own by asking Claude Code to update its own cache, so
  the meters no longer need you to run `/usage` by hand.
- Usage caches are read once a minute (immediately after a refresh), and each
  account has a backoff — an account with no readable cache used to retry
  forever.
- Claude session markers inherited from a parent Claude are scrubbed from tab
  environments; without this, tabs launched from inside a Claude session
  reported "Transcript saving is off" and silently could not resume.
- Resume runs in the directory the transcript recorded, not the tab's current
  one, which is why some resumes reported "No conversation found".
- A declined permission prompt no longer leaves the attention flag stuck.
- Hook and statusline paths self-heal after `cargo install` or a moved binary,
  and a second instance never steals a live instance's hook socket.

### Fixes
- Rail glyphs (spinner, flag, ✓) rendered as tofu boxes — egui's built-in
  fonts have no braille or symbol coverage; the terminal faces are now
  installed for UI text too.
- `doctor` and the statusline no longer assume Unix, so Windows builds.
- Release workflow creates the GitHub release before uploading to it.

### Not yet
Split panes, SSH/remote tabs, a plugin system, a settings screen, and hardware
passes on macOS and Windows.

## v0.1.0 — 2026-07-29

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
