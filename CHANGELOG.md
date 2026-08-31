# Changelog

## v0.6.2 — 2026-08-31

- Usage numbers follow the cache instead of a timer. Claude Code rewrites the
  cache every time a session fetches usage; Giverny re-read it once a minute,
  so a number could sit a minute behind a file that already had the new one.
  The files are now checked for having been rewritten every couple of seconds —
  one `stat` per account — and re-read the moment they are.

- The panel always says how old the numbers are. It only said so past half an
  hour, which left the first half hour — the part anyone actually watches —
  with nothing to read. Amber once they are properly old.

- A finished tab is green, not cyan. Done and needs-you were a tick and a flag
  in two colours from the same family; they are now told apart by colour and
  by movement as well as by shape.

## v0.6.1 — 2026-08-31

- A tab inside WSL reopens where it was — actually, this time. The sweep that
  asks a distribution where its tabs are passed its script as an argument to
  `wsl.exe`, and a command line crossing that boundary is rebuilt on the way
  through: this one is quotes, newlines and `$` from end to end, and it came
  back empty every time on a machine where running the same script by hand
  worked. The script now travels as a file over the same share the account is
  read through, and is run by path. Nothing on the Windows side had ever
  learned a WSL tab's directory, so every tab reopened at `~`.

- A sweep whose thread died no longer stops every later one. It held the
  receiver until an answer came, and a thread that never answered left it held
  forever.

- `giverny doctor` prints what the sweep sees per distribution: the tabs it
  reports and where they are. Zero while Giverny is open, with tabs in that
  distribution, is the sweep failing.

## v0.6.0 — 2026-08-31

- The rail groups tabs by repository, as well as by category. Two segments at
  the top switch between them, and which one you left it in is remembered with
  the window size and the rail width. Repository groups are built from where
  the tabs are: named after the checkout, sorted by name, each keeping the same
  colour every launch, with tabs in no repository gathered at the end. Under a
  repository, a tab's path says where *in* the repository it is rather than
  repeating the name already on the header, and the full path is on the header's
  hover. Dragging tabs and adding categories stay in the categories view, where
  they mean something — a repository is a place tabs are, not one they can be
  put in. A "new tab" from a repository header opens in that repository.

- Every tab shows its git branch, not only the one you last looked at. The
  branch came from the same refresh that reads the active tab's directory out
  of `/proc`, so a tab that had never been focused had none. It is now read
  once per repository on the same sweep, which also gives a tab inside WSL a
  branch for the first time: its directory is a unix path, which Windows can
  only reach through the distribution's share.

## v0.5.7 — 2026-08-31

- Logging in inside a WSL tab works again. Giverny listed `CLAUDE_CONFIG_DIR`
  in `%WSLENV%` whether or not it was setting one, and a name listed there
  with nothing behind it does not arrive absent — it arrives empty. Claude
  Code saw `CLAUDE_CONFIG_DIR=` and took it at its word, so `/login` said
  "Login successful", wrote the credentials, and the next thing to look for
  them looked somewhere else: "Not logged in · Please run /login", forever.
  Only variables actually being set are listed now.

- An empty entry in an inherited `%WSLENV%` no longer becomes a variable.
  Windows Terminal exports a trailing colon, which the merge turned into a
  nameless entry.

## v0.5.6 — 2026-08-30

- A tab inside WSL now has a directory to remember. v0.5.5 taught tabs to
  reopen where they were, which did nothing, because nothing on the Windows
  side ever learned where that was: `/proc` is on the other side of the
  boundary, `wsl.exe`'s own working directory is not the shell's, and a WSL
  bash emits no OSC 7 unless something configured it to. The distribution is
  asked instead — every process Giverny starts carries `GIVERNY_TAB_ID` in its
  environment, so one `sh` per distribution reports where each tab's shell is.
  It runs every five seconds, off the UI thread, and costs nothing on a
  machine with no distributions.

## v0.5.5 — 2026-08-30

- A tab inside WSL reopens where it was, instead of going home every time. A
  WSL shell reports its directory over OSC 7 as the unix path it is, and every
  spawn threw that away: Windows cannot open `/home/x/proj`, so the tab fell
  back to the Windows home and the `~` on the command line sent it to the
  Linux home anyway. The remembered directory is now checked over the share
  and passed as `--cd` — checked because `--cd` on a directory that has since
  been deleted is a tab that does not open at all. A new tab opened from a WSL
  tab lands in the same directory, which is what it does everywhere else.

- The startup directory check no longer runs for a tab inside a distribution.
  It compared the shell's directory against the one it was spawned with, and
  across the boundary those are two different machines' answers: all it could
  ever do is type a Windows path at a bash prompt.

## v0.5.4 — 2026-08-30

- Windows tabs open again. v0.5.3 opened them with `wsl.exe -d <distro> ~`,
  and `~` is a shorthand rather than an argument: alone it means "start in the
  Linux home", but with `-d` in front of it wsl.exe reads it as the command to
  run, which bash expands and tries to execute — `/home/…: Is a directory`,
  then a dead tab. The distribution is only named when it is not the default
  one, and naming it now uses `--cd`, which is the documented flag for the
  same thing. A distribution whose name has a space is quoted, since arguments
  reach `CreateProcess` joined and unescaped.

- A session is no longer told the path of its own default account. Setting
  `CLAUDE_CONFIG_DIR` does more than point at a directory: it also moves where
  Claude Code keeps its identity, from beside the directory to inside it. A
  session handed the path of the account it would have picked anyway would
  have come up logged out, staring at a fresh login. Giverny says nothing and
  lets it find that account by itself; which account a tab is on now travels
  as `GIVERNY_PROFILE_DIR`, which changes nothing for Claude Code and is what
  hooks carry back out.

## v0.5.3 — 2026-08-30

- A Claude Code that lives inside WSL is a Claude Code Giverny can see. On
  Windows it is the normal setup — Giverny opens tabs in a WSL login shell,
  and everything the agent writes lands in that distribution's `~/.claude`,
  which is a different directory from the `~/.claude` on the Windows side that
  Giverny was reading. The account looked empty, the meters had nothing to
  show, and `claude -p /usage` had no program to run: "usage refresh skipped:
  claude is not on %PATH%".

  Accounts inside distributions are now found by asking `wsl.exe` what it has
  and each distribution where its home is, and read straight off
  `\\wsl.localhost\<distro>\home\<user>\.claude`. Refreshing runs `claude`
  inside the distribution. Hooks and the statusline install a command the
  distribution can execute — this binary, through `/mnt/c/…`, asked of
  `wslpath` rather than assumed — and the tab identity crosses both ways
  through `%WSLENV%`, so a hook fired in WSL reaches the Windows app and lands
  on the right tab. A tab whose category names such an account opens in that
  account's distribution, and sessions are named the unix path they can open
  while Giverny keeps storing the Windows one. Nothing to configure: update
  and the account appears.

  Still not the API. Usage comes from Claude Code's own cache, refreshed by
  asking Claude Code — no credential is read and no request is made, which
  stays true whichever side of the boundary the account is on.

- The identity file is found by layout rather than by whose home it is. A
  directory named `.claude` keeps it beside itself (`~/.claude.json`); a named
  `CLAUDE_CONFIG_DIR` keeps it inside. The rule was "is this exactly my own
  `~/.claude`", which no account inside WSL can ever be. The order is deliberate
  where both files exist: a home that once ran with `CLAUDE_CONFIG_DIR` pointed
  at its own `~/.claude` has an inner file left over and months stale.

- Session liveness for an account inside WSL is the registry file's own
  freshness, not its pid. The pids in it are another machine's, and a
  collision with a Windows pid is enough to refuse to restore a tab.

- Picking a tab leaves the settings screen and the key list. They take the
  terminal's place, so clicking a tab in the rail with either open looked like
  a click that did nothing, and the only way out was the button that opened it.

- `giverny doctor` reports the WSL side: every distribution, the `claude` it
  found there, and the account directory.

## v0.5.2 — 2026-08-30

- The shell a tab opens on Windows is a setting, `behavior.windows_shell`:
  `auto`, `wsl`, `powershell` or `cmd`. `auto` still prefers a WSL login
  shell, which is where Claude Code and unix tooling usually live — but only
  when a distribution is actually installed. `wsl.exe` ships with Windows
  whether or not there is anything behind it, so the old check ("is wsl.exe on
  %PATH%?") was true on machines with no distribution at all, and those tabs
  opened onto "no installed distributions" instead of a shell. It now asks WSL
  what it has, once per run, and falls back to PowerShell when the answer is
  nothing. A machine with both can be told which one it wants.

- `Ctrl+Tab` switches between tabs by when you last used them, not where they
  sit in the rail: one press is "back to the tab I came from", and holding
  `Ctrl` down while pressing it again keeps walking back through the ones
  before that, `Ctrl+Shift+Tab` forward again. The walk commits when `Ctrl`
  comes up, and only the tab it lands on moves to the front of the order —
  stepping past a tab is not using it, so pressing twice always means two
  back. The order is kept with the tabs, so it survives a restart, and every
  way of changing tabs feeds it. `Ctrl+PageUp`/`PageDown` still walk the rail
  in order.

- Usage numbers refresh on Windows. Giverny asks Claude Code to update them by
  running `claude -p /usage`, and Rust resolves a bare program name on Windows
  by appending `.exe` and nothing else — no `%PATHEXT%` — so an npm-installed
  `claude.cmd` was never found and every refresh ended in "usage refresh
  skipped: program not found", leaving the meters on whatever the cache last
  held. The executable is now resolved here: `%PATH%` first, then `~\.local\bin`
  and `%APPDATA%\npm` for the window after an install, and a `.cmd` or `.bat`
  shim is run through `cmd /c`, which is the only way `CreateProcess` will run
  one. When there is genuinely no `claude` to run, the log now says so instead
  of naming a program it never names.

## v0.5.1 — 2026-08-18

- An out-of-memory kill in one tab no longer closes the whole terminal.
  Everything a tab starts — a shell, an agent, a dev server, a headless
  browser — runs inside the systemd scope the launcher created for Giverny,
  and an app scope defaults to `OOMPolicy=stop`: when the kernel's OOM killer
  takes any process in it, systemd stops the entire unit, so Giverny is handed
  a `SIGTERM` and every other tab dies for the sake of the one process that
  was picked. `install-desktop` now writes a drop-in that sets
  `OOMPolicy=continue`, which leaves the rest of the scope alone. The one-line
  installer already runs that command, so updating is enough; from a source
  build, run `giverny install-desktop` once. `giverny doctor` prints the
  policy actually in force, and a scope still set to `stop` says so in the log
  at startup. Linux only: there is no scope to stop on macOS or Windows.

## v0.5.0 — 2026-08-12

- Scrollback is saved while the app is running, not only as it closes. It was
  written in one place, on the way out, so the exits that skip that code —
  a kill, an out-of-memory, a compositor that takes the session with it — came
  back showing whatever the tab held at the last clean quit. One tab is now
  snapshotted every couple of seconds, the one longest without, skipping any
  whose screen has not changed. A kill costs a tab a minute of output.
- `SIGTERM`, `SIGINT` and `SIGHUP` shut down the way closing the window does.
  Nothing was listening for them, and systemd stops the app's scope with
  `SIGTERM` at logout, so an ordinary end of session skipped every write the
  shutdown path makes.
- The clean-shutdown marker is true. It was set unconditionally by a path that
  also runs when the event loop errors out, so losing the compositor recorded
  itself as an ordinary quit; a start that finds the previous run unclean now
  says so.

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
