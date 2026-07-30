# Architecture

Cargo workspace, four crates:

```
crates/
├─ app/      eframe shell: rail UI, usage panel, Claude state machine, actions
├─ term/     terminal engine: PTY + forked event loop + byte tee +
│            alacritty_terminal Term + glyph-atlas renderer + input encoders
├─ core/     tabs/categories model, persistent state, git info, paths
└─ claude/   Claude Code integration: profiles, session registry, hook
             relay/installer, usage cache parser
```

## The terminal engine (`crates/term`)

- **`io_loop.rs`** is a fork of `alacritty_terminal`'s `event_loop.rs` with three deltas: every PTY byte is observed by the **tee** before `Term::advance`; `Event::PtyWrite` replies (DA1/DA2/CPR/kitty-flags probes) short-circuit into the loop's own write queue instead of round-tripping the UI thread; and a `LoopDone` callback fires after the EOF drain. Upstream's sync-timeout poll deadline (DECSET 2026) and per-lock read caps are preserved.
- **`tee.rs`** is a second `vte::Parser` that extracts what `alacritty_terminal` doesn't surface: OSC 7 (cwd, hostname-verified), OSC 133 prompt marks, OSC 9/777 notifications, and Giverny's private OSC 7791 (nonce-authenticated; usable by hooks' `terminalSequence`). The tee never modifies the stream.
- **Rendering**: `Snapshot::capture` copies the viewport (colors fully resolved) under the terminal lock; mesh building — including first-use swash rasterization into shelf-packed atlas pages — happens outside it. Cell metrics are integer physical pixels, which removes subpixel bins and seam shimmer. Never per-cell egui galleys.
- **Input**: legacy xterm + kitty CSI-u encoders (`Shift+Enter` ⇒ `ESC[13;2u` when Claude enables the kitty protocol), SGR/legacy mouse reporting, sanitized bracketed paste.

### Bidirectional text

The grid stores logical order, always: it is what the program wrote and what it expects to read back, and selection, search and the resume machinery all index it. Display is where direction is applied — `render/bidi.rs` reorders each row that contains RTL script, and the mesh draws cells at their visual columns. This is VTE's approach, and the reason Hebrew looks right in GNOME Terminal.

Rows with no RTL character skip the algorithm entirely, so the cost for ordinary output is one range test per cell. Mouse hit-testing and selection still work in logical columns, so a click inside an RTL run currently lands on the wrong cell — the mapping exists (`logical_to_visual`) and inverting it is the next step.

## Persistence (`crates/core`)

`~/.config/giverny/state/tabs.json` (versioned, atomic tmp+rename, corruption sidelined) plus per-tab ANSI scrollback dumps. Restore **pre-seeds** the fresh `Term` with the dump before the shell spawns — scrollback returns with colors and re-wraps naturally because dumps store *logical* lines (WRAPLINE rows joined). Alt-screen content is never snapshotted. Sessions spawn lazily on first focus.

Window size, maximized state and rail width live in the same file under `layout`, read by `state::load_layout` *before* the window is built so the size is what the window opens at rather than a resize the user watches happen. The size comes from egui's viewport rect, not `ViewportInfo::inner_rect` — the latter is computed from the window's position, which Wayland never reports to clients, so it is `None` on the primary platform. Window *position* is not persisted for the same reason: Wayland gives clients no way to place their own windows.

## Claude integration (`crates/claude` + `app/claude_watch.rs`)

Two independent state sources, merged per tab:

1. **Hooks** (rich, crisp): `giverny relay` registered for `SessionStart / UserPromptSubmit / Stop / Notification / SessionEnd`. The relay forwards stdin JSON + `$GIVERNY_TAB_ID` + `$CLAUDE_CONFIG_DIR` to a unix socket, spooling to disk when the app is closed. `notification_type` taxonomy: `permission_prompt`/`elicitation_dialog`/`agent_needs_input` ⇒ **needs-you**; `idle_prompt`/`agent_completed`/`task_completed` ⇒ done.
2. **Registry** (zero-config baseline): `$CONFIG_DIR/sessions/<pid>.json`, written by Claude Code itself (`status: busy|idle`), PID-liveness-gated, mapped to tabs by walking `/proc` parent chains to the tab's shell.

Hook-set attention states are stickier than registry state; done-markers clear when the user views the tab.

**Resume**: tabs persist `(claude_session, claude_config_dir)`. Restore injects `CLAUDE_CONFIG_DIR=… command claude --resume <uuid>` after a settle delay (`command` bypasses shell wrapper functions), guarded by a registry check so a session live in another terminal is never double-resumed (double-resume interleaves one transcript — documented Claude Code behavior).

**Usage**: parsed from `.claude.json → cachedUsageUtilization.limits[]` only (kind `session`/`weekly_all`/`weekly_scoped`, scoped model names like *Fable*, server-side `severity`). Windows whose `resets_at` has passed render 0% (verified server quirk: lapsed windows keep stale percentages). No network, no tokens — and deliberately no `/api/oauth/usage` polling (unsupported endpoint, ToS-exposed) and **never** an OAuth refresh (refresh tokens are single-use; a third-party refresh force-logs-out Claude Code).

## Threading

UI thread (egui, reactive repaints) + one PTY io thread per live tab + hook-listener thread + short-lived helpers. No async runtime. `Wakeup` events coalesce on an atomic edge; a 128 KiB advance cap per lock hold keeps the UI responsive under floods (tested at 20 MB bursts).
