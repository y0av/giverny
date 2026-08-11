# Changelog

## Unreleased

- A tab whose conversation was never recorded recovers it from the command it
  remembers. `claude --resume <id>` names its own conversation, and that was
  the one place a crash left the id written down — sitting in the tab's saved
  foreground command, ignored. It is adopted onto the tab, so the next save
  has it whether or not the resume runs.

- Typing through an input method works. Hebrew, Arabic, CJK — anything the
  platform IME composes — arrives as an IME commit rather than a text event,
  and the terminal matched only text: every composed character was dropped on
  the floor. Pre-edit is still not forwarded, since the composition is not
  final and the program on the other end of the pty owns the line.
- Hebrew and Arabic are legible. DejaVu Sans *Mono* covers neither, so both
  fell through to the proportional DejaVu Sans and were drawn into monospace
  cells. Script-specific faces (Miriam Mono CLM, Noto Sans Hebrew, Noto Naskh
  / Sans Arabic) are now tried first; they can only serve the scripts they
  cover, so nothing else changes.

- Ctrl+click opens a link once. Giverny opened what was under the pointer and
  then forwarded the same click to the program, and Claude Code opens
  hyperlinks itself (`onHyperlinkClick` → `xdg-open`) — so two browser tabs.
  Ctrl over a path or URL now claims the pointer: the click is ours, and no
  longer reported to the application.

- Tabs remember the conversation they are holding even when it started before
  hooks were installed. `tab.claude_session` — the thing restore resumes — was
  written only by the `SessionStart` hook, so a session already running when
  hooks landed was never recorded, and came back after a restart as a plain
  shell. The registry names that session every second and the rail was showing
  it the whole time; now it is saved too.

- A new tab opens the category it lands in. Creating one in a collapsed
  category left the rail with no selection anywhere while you typed into it,
  since the new tab is the active one. Applies to `+` on a collapsed header,
  `Ctrl+Shift+T` into a collapsed active category, and attaching to a
  background agent.

- Two Claude settings (settings → Claude):
  - **start Claude in auto mode** writes `permissions.defaultMode = "auto"`
    into each account's `settings.json`. Claude Code's own setting, so it
    applies however you start a session — typed, resumed, or attached — not
    only to ones Giverny launches. Turning it off removes the key again, and
    refuses to touch a mode you set by hand.
  - **resume conversations whole** answers Claude Code's "Resume from summary
    (recommended)" / "Resume full session as-is" prompt with as-is, every
    time. It appears for a session over 70 minutes old and 100k tokens, and
    both thresholds come from the environment, so tabs are spawned with them
    out of reach rather than the prompt being answered for you.

- A background shell no longer reads as Claude working. Claude Code publishes
  four session statuses — `busy`, `shell`, `idle`, `waiting` — and its own
  session list counts `shell` as working. Measured against a live session,
  `busy` holds through minutes of back-to-back tool calls, so `shell` does not
  mean "running a command": it means a shell is still alive while the agent
  waits at its prompt, usually with a question for you. A tab in that state now
  shows a static ⠿ beside an idle marker instead of a spinner — animating it
  said "come back later" about the tab most likely to want you.
- A session blocked on the user (`waiting`) raises the attention flag. With no
  hook to report it — every session started before hooks were installed — it
  used to read as idle.
- A subagent finishing no longer ends the turn. `agent_completed` and
  `task_completed` fire mid-turn, while the main agent carries on with the
  result; they were treated as "the session finished" and stopped the spinner
  half way through the work. Only `idle_prompt` — or `Stop` — means the prompt
  is back.

## v0.4.0 — 2026-08-02

### Terminal
- Images. The kitty graphics protocol is implemented: transmit (inline base64
  or from a file), display, delete, and the support query programs use to
  decide whether to bother — PNG and raw RGB/RGBA, reassembled from the 4 KiB
  chunks real senders use. Not implemented: animation, shared memory, z-index,
  Unicode placeholders.
  Images are applied *mid-stream*, with the terminal advanced exactly as far as
  the escape and no further, so an image lands on the cursor the program left
  it on rather than wherever the following output ended up. They scroll with
  their text and are dropped once it leaves the scrollback.

- Hebrew and Arabic render the right way round. The grid keeps text in logical
  order — that is what programs read back — and rows containing RTL script are
  reordered for display with the Unicode Bidi Algorithm, the way VTE does it.
  Mixed lines only reorder the RTL run, and digits inside it stay readable. A
  row with no RTL character never reaches the algorithm, so Latin output costs
  one character test per cell. The cursor follows the reordering.
  Not yet: clicking or drag-selecting inside an RTL run maps to the wrong cell,
  because selection still works in logical columns.

- Dropping files into a tab types their paths, quoted for the shell and routed
  through the same sanitized, bracketed paste as Ctrl+V — a filename is
  untrusted input. It is how you hand Claude an image.
  It works on Wayland now, where it never did: winit 0.30 — the version egui
  pins — creates no `wl_data_device` at all, so a drag over the window was
  never offered to us. Binding a second data device is the obvious fix and is
  a trap: Mutter keeps one per client and evicts the old one, so ours would
  have taken the clipboard's selection events with it and paste would have
  stopped working — visible in `meta-wayland-data-device.c`, and on the wire
  under `WAYLAND_DEBUG=1`. The one device this process has belongs to the
  clipboard, so drags come from there: `vendor/smithay-clipboard` is upstream
  0.7.3 with the four drag handlers it leaves empty filled in.
  Because the drag is tracked here, we know where the pointer is: the tab
  under it highlights, and that is the tab the paths are typed into. X11 drops
  into the active tab, since winit reports no position there.

- `Ctrl+Shift+E` labels every path and URL on screen. A letter opens it;
  Shift+letter types it at the cursor. It reads the rendered screen, so it
  works inside a full-screen program like Claude — where the shell's own
  completion does not exist, because the shell is not running. Same detector as
  Ctrl+click. If there are more targets than labels it says how many were left
  out rather than quietly dropping them.

- OSC 8 hyperlinks are clickable. Ctrl+click already handled bare URLs and
  file paths in the visible text, but a link emitted as OSC 8 keeps its URL in
  the escape sequence and shows only a label, so there was nothing in the text
  to match — which is how Claude, `gh` and `ls --hyperlink` print links. The
  cell's link metadata is now consulted first, and hovering underlines the
  whole label rather than one word.

### Claude Code
- Background agents appear in the rail. Claude Code runs work with no tab —
  `/fork` into a background session, `run_in_background` commands, agents that
  outlive the session that started them — and until now the one Claude you are
  most likely to forget was the only one Giverny could not see. A BACKGROUND
  section lists them with the same grammar as tabs: spinner while working, ⚑
  when blocked, ✓ when done. Clicking one opens a tab attached to it, in the
  agent's own directory and account.
  A job whose state file says "working" but whose worker process is gone reads
  as *stale* rather than spinning forever.

### Fixes
- `Ctrl+C` interrupts again. egui-winit converts the platform clipboard chords
  into `Copy`/`Cut` events and returns without emitting the key, so on Linux and
  Windows — where that chord is `Ctrl+C` — nothing reached the child: no
  interrupt, and no copy either, since `Ctrl+Shift+C` was equally swallowed
  (auto-copy-on-selection hid that). Shift now separates them: `Ctrl+Shift+C`
  copies, `Ctrl+C` sends ETX, `Ctrl+X` sends CAN, and on macOS `Cmd+C` still
  copies.
- The rail's `+` and close buttons no longer sit under the scrollbar once
  there are enough tabs to scroll. egui's floating scrollbars are drawn over
  the last 10px of content; that width is now reserved when a bar is shown.
- `behavior.prefer_x11` runs Giverny under X11/XWayland on Linux. It was how
  drops used to arrive and is now rarely needed; if X11 turns out to be
  unavailable, Giverny restarts itself on Wayland rather than refusing to
  start.

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
