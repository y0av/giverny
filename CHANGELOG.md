# Changelog

## Unreleased

- OSC 8 hyperlinks are clickable. Ctrl+click already handled bare URLs and
  file paths in the visible text, but a link emitted as OSC 8 keeps its URL in
  the escape sequence and shows only a label, so there was nothing in the text
  to match — which is how Claude, `gh` and `ls --hyperlink` print links. The
  cell's link metadata is now consulted first, and hovering underlines the
  whole label rather than one word.

- Hebrew and Arabic render the right way round. The grid keeps text in logical
  order — that is what programs read back — and rows containing RTL script are
  reordered for display with the Unicode Bidi Algorithm, the way VTE does it.
  Mixed lines only reorder the RTL run, and digits inside it stay readable. A
  row with no RTL character never reaches the algorithm, so Latin output costs
  one character test per cell. The cursor follows the reordering.
  Not yet: clicking or drag-selecting inside an RTL run maps to the wrong cell,
  because selection still works in logical columns.

- `Ctrl+C` interrupts again. egui-winit converts the platform clipboard chords
  into `Copy`/`Cut` events and returns without emitting the key, so on Linux and
  Windows — where that chord is `Ctrl+C` — nothing reached the child: no
  interrupt, and no copy either, since `Ctrl+Shift+C` was equally swallowed
  (auto-copy-on-selection hid that). Shift now separates them: `Ctrl+Shift+C`
  copies, `Ctrl+C` sends ETX, `Ctrl+X` sends CAN, and on macOS `Cmd+C` still
  copies.

- Dropping files into a tab types their paths, quoted for the shell and routed
  through the same sanitized, bracketed paste as Ctrl+V — a filename is
  untrusted input.
- `behavior.prefer_x11` runs Giverny under X11/XWayland on Linux, which is how
  drops arrive today: the winit egui pins (0.30) has no Wayland drop support.
  Wayland DnD is implemented in winit master and reaches us with egui's winit
  0.31 migration. If X11 turns out to be unavailable, Giverny restarts itself
  on Wayland rather than refusing to start.
- A drop lands in the active tab, and the hint says which one. Aiming a drop at
  a specific tab needs a drag position, which this winit does not report (X11
  reads the XdndPosition coordinates and discards them). winit master reports
  positions, so per-tab targeting becomes possible with the same upgrade.

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
