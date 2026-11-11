# Architecture

Cargo workspace, five crates:

```
crates/
├─ app/      eframe shell: rail UI, usage panel, Claude state machine, actions
├─ term/     terminal engine: PTY + forked event loop + byte tee +
│            alacritty_terminal Term + glyph-atlas renderer + input encoders
├─ core/     tabs/categories model, persistent state, git info, paths
├─ claude/   Claude Code integration: profiles, session registry, hook
             relay/installer, usage cache parser
└─ codex/    Codex hook installer, bounded app-server RPC, usage windows/cache
```

## The terminal engine (`crates/term`)

- **`io_loop.rs`** is a fork of `alacritty_terminal`'s `event_loop.rs` with three deltas: every PTY byte is observed by the **tee** before `Term::advance`; `Event::PtyWrite` replies (DA1/DA2/CPR/kitty-flags probes) short-circuit into the loop's own write queue instead of round-tripping the UI thread; and a `LoopDone` callback fires after the EOF drain. Upstream's sync-timeout poll deadline (DECSET 2026) and per-lock read caps are preserved.
- **`tee.rs`** is a second `vte::Parser` that extracts what `alacritty_terminal` doesn't surface: OSC 7 (cwd, hostname-verified), OSC 133 prompt marks, OSC 9/777 notifications, and Giverny's private OSC 7791 (nonce-authenticated; usable by hooks' `terminalSequence`). The tee never modifies the stream.
- **Rendering**: `Snapshot::capture` copies the viewport (colors fully resolved) under the terminal lock; mesh building — including first-use swash rasterization into shelf-packed atlas pages — happens outside it. Cell metrics are integer physical pixels, which removes subpixel bins and seam shimmer. Never per-cell egui galleys.
- **Input**: legacy xterm + kitty CSI-u encoders (`Shift+Enter` ⇒ `ESC[13;2u` when Claude enables the kitty protocol), SGR/legacy mouse reporting, sanitized bracketed paste.

### Images

The kitty graphics protocol arrives as APC (`ESC _ G … ESC \\`), which `vte` has no callback for and `alacritty_terminal` does not implement — so `tee.rs` scans the raw bytes for it alongside the parser, and reports the byte offset where each sequence ended.

That offset is the whole point. An image is placed at the cursor, so `io_loop` advances the terminal up to the escape, applies the command with the cursor exactly where the program left it, then advances the rest. Handling it a read later — even one frame later on the UI thread — puts the image wherever the following output happened to end up, which is what the first implementation did.

Placements are anchored to `history_size + cursor_line` so they scroll with their text, and are pruned when it leaves the scrollback. Beyond the scrollback cap that anchor drifts; it is exact for everything still in history.

### Dropped files

Wayland has one drag-and-drop channel, `wl_data_device`, and Mutter keeps
**one per client** — `get_data_device` evicts the client's previous device
from the list it resolves drags against. winit 0.30 creates none, so drops
never arrived; creating our own would have taken the clipboard's selection
events with it and broken paste. Both facts were read from
`meta-wayland-data-device.c` and confirmed on the wire with `WAYLAND_DEBUG=1`.

So the drags arrive on the device the clipboard already owns:
`vendor/smithay-clipboard` is upstream with its empty drag handlers filled in,
applied through `[patch.crates-io]` so eframe and Giverny share one copy. It
goes away when egui moves to winit 0.31. Paths are typed through the same
sanitized bracketed paste as `Ctrl+V` — a filename is untrusted input.

### Bidirectional text

The grid stores logical order, always: it is what the program wrote and what it expects to read back, and selection, search and the resume machinery all index it. Display is where direction is applied — `render/bidi.rs` orders the rows of a screen that has RTL script on it, and the mesh draws cells at their visual columns. This is VTE's approach, and the reason Hebrew looks right in GNOME Terminal.

A row runs in the direction of its first letter (Unicode's rule P2, a browser's `dir="auto"`), and a right-to-left row is mirrored across the whole width, so a Hebrew answer sits against the right edge with Claude's `⏺` and indent on the right, and its words, English terms and full stops in reading order. Left to right for every row would put the runs of a mixed sentence in the wrong order. The exception is a row that carries on the paragraph above it, which takes that paragraph's direction: Claude Code wraps its own text, so the terminal sees no soft wrap, and the second row of a Hebrew sentence can start with an English word. A row carries on the one above when it starts at that row's text column, opens no bullet or message of its own, and its first word would not have fitted at the end of the row above. Lines that merely follow each other, like code, fail the last test. Box drawing is a wall: the text between each pair is ordered on its own and the walls stay put, so a table whose rows run in different directions keeps its columns.

Screens with no RTL character skip all of it, so the cost for ordinary output is one range test per cell. Each snapshot carries the orders of the rows it moved, and the widget inverts them for the pointer: clicks, selection, mouse reporting and link hover all work in logical cells.

egui, which draws the rail and the prompt bar, has no bidi pass, but it shapes each run of one font with HarfBuzz's guess at its direction, so it turns a Hebrew word round by itself and leaves a sentence's words in the wrong order. `bidi::egui_line` orders the line and then puts each RTL run back in writing order for egui to finish; a test pins egui's half of that.

Hebrew glyphs come from Cascadia Mono, bundled in `crates/term/fonts`: monospace Hebrew designed for a coding font, where most systems have only proportional faces or Miriam Mono. They serve the Hebrew block and nothing else, so no other character changes face.

## Splits and windows

A rail row is a tab, or a **group**: several tabs laid out by a binary split
tree (`core/panes.rs`, bspwm style; each split has an axis and a ratio). A
tab stays one terminal throughout, so hooks, watchers, snapshots and
`GIVERNY_TAB_ID` never learned about splits. Group members sit next to each
other in `Workspace::tabs`, which is all the rail needs to draw a group once;
moving or reordering acts on the whole block. `ws.active` is the focused
terminal, and "seen" means visible: every terminal of the active row.

More windows are egui immediate viewports. Each shows the same workspace with
its own focus. Drawing one swaps its `Focus` (active tab and recency) and its
on-screen geometry into `App`, runs the same `window_ui` the first window runs,
and swaps back, so every tab operation works in every window with no second
copy. Immediate viewports draw whenever the root does; output in any terminal
repaints the root and with it every window. A row is shown in at most one
window, since a terminal has one size: choosing it in one window takes it from
the others (`windows::settle`). Overlays belong to the window they opened in.

## Persistence (`crates/core`)

`~/.config/giverny/state/tabs.json` (versioned, atomic tmp+rename, corruption sidelined) plus per-tab ANSI scrollback dumps. Restore **pre-seeds** the fresh `Term` with the dump before the shell spawns — scrollback returns with colors and re-wraps naturally because dumps store *logical* lines (WRAPLINE rows joined). Alt-screen content is never snapshotted. Sessions spawn lazily on first focus.

Window size, maximized state and rail width live in the same file under `layout`, read by `state::load_layout` *before* the window is built so the size is what the window opens at rather than a resize the user watches happen. The size comes from egui's viewport rect, not `ViewportInfo::inner_rect` — the latter is computed from the window's position, which Wayland never reports to clients, so it is `None` on the primary platform. Window *position* is not persisted for the same reason: Wayland gives clients no way to place their own windows. Extra windows keep their focus, size and rail state under `windows`.

## Claude integration (`crates/claude` + `app/claude_watch.rs`)

Two independent state sources, merged per tab:

1. **Hooks** (rich, crisp): `giverny relay` registered for `SessionStart / UserPromptSubmit / Stop / Notification / SessionEnd`. The relay forwards stdin JSON + `$GIVERNY_TAB_ID` + `$CLAUDE_CONFIG_DIR` to a unix socket, spooling to disk when the app is closed. `notification_type` taxonomy: `permission_prompt`/`elicitation_dialog`/`agent_needs_input` ⇒ **needs-you**; `idle_prompt`/`agent_completed`/`task_completed` ⇒ done.
2. **Registry** (zero-config baseline): `$CONFIG_DIR/sessions/<pid>.json`, written by Claude Code itself (`status: busy|idle`), PID-liveness-gated, mapped to tabs by walking `/proc` parent chains to the tab's shell. For an account inside WSL the pids are another machine's, so liveness there is the entry file's own freshness instead.

Hook-set attention states are stickier than registry state; done-markers clear when the user views the tab.

**Resume**: tabs persist `(claude_session, claude_config_dir)`. Restore injects `CLAUDE_CONFIG_DIR=… command claude --resume <uuid>` after a settle delay (`command` bypasses shell wrapper functions), guarded by a registry check so a session live in another terminal is never double-resumed (double-resume interleaves one transcript — documented Claude Code behavior).

**Usage**: parsed from `.claude.json → cachedUsageUtilization.limits[]` only (kind `session`/`weekly_all`/`weekly_scoped`, scoped model names like *Fable*, server-side `severity`). Windows whose `resets_at` has passed render 0% (verified server quirk: lapsed windows keep stale percentages). No network, no tokens — and deliberately no `/api/oauth/usage` polling (unsupported endpoint, ToS-exposed) and **never** an OAuth refresh (refresh tokens are single-use; a third-party refresh force-logs-out Claude Code). Refreshing means asking Claude Code to do it: `claude -p /usage`, run wherever that account's Claude Code lives — inside the distribution, for an account in WSL.

## Codex integration

`crates/codex` isolates the documented Codex interfaces. App-side
`codex_watch` tracks per-tab state and runs usage and restore metadata requests
on background workers. The existing relay transport carries an explicit provider;
legacy messages still default to Claude. Shared presentation states live in core,
while Claude registry merging and account logic remain provider-specific.

Codex hooks carry session ID, tab ID, home, timestamp and WSL distribution.
They only observe; they return no model context or permission decision. User hook
trust remains Codex's responsibility. The runtime rejects stale session events,
ignores subagent events and never associates conversations by cwd alone.

A bounded stdio JSON-RPC helper initializes with Giverny's identity and reads
account limits without starting turns. It owns authentication, including any
network access. Giverny persists only returned allowance windows in its own
cache. Cancellation kills and reaps owned helpers; no user daemon is stopped.

Restoration reads metadata without resuming it, then reconnects the normal CLI
to the saved conversation. Loaded daemon threads do not block restoration;
the prompt mode offers an explicit resume action. Persistence adds an optional provider-tagged
session reference while retaining legacy Claude fields and state version.

## Threading

UI thread (egui, reactive repaints) + one PTY io thread per live tab + hook-listener thread + short-lived helpers. No async runtime. `Wakeup` events coalesce on an atomic edge; a 128 KiB advance cap per lock hold keeps the UI responsive under floods (tested at 20 MB bursts).
