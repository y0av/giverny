<img src="assets/icon/giverny-128.png" width="96" height="96" alt="Giverny">

# Giverny

**A native GPU terminal built around Claude Code.** Categorized, persistent tabs on a left rail; live Claude activity on every tab; usage meters for all your Claude accounts. *Where your Claudes live.*

Named for [Giverny](https://en.wikipedia.org/wiki/Giverny), Claude Monet's garden village — a home for the other famous Claude.

![Giverny: categorised tabs with live Claude state and multi-account usage meters](assets/demo.gif)

*One tab is working (spinner), one finished while you were away (✓), one is waiting on you (⚑) — and every account's limits are in the corner. Mock data; regenerate with [`tools/demo`](tools/demo).*

## Why

Running many concurrent Claude Code sessions across projects — and across multiple subscription accounts — in ordinary terminals means losing track of which session is working, which is waiting on *you*, and how close each account is to its rate limits. Giverny is a real terminal emulator (Rust, GPU-rendered, `alacritty_terminal` core, no web engine) whose entire chrome is built for that workflow.

## Features

- **A real terminal first.** Kitty keyboard protocol (Shift+Enter newline in Claude works out of the box — no `/terminal-setup`), synchronized output (flicker-free Claude redraws), bracketed paste with escape-sequence sanitizing, SGR mouse reporting, selection with auto-copy (double-click word, triple-click line), OSC 52 (store capped, clipboard *reads* denied), truecolor + glyph atlas rendering with integer-pixel cell metrics.
- **Vertical, categorized tabs.** Categories with Monet-palette colors (right-click to rename, recolor, assign an account, delete); drag tabs to reorder or move them between categories; tabs show live cwd (OSC 7 + process fallback), git branch (worktree-aware), and the Claude account they run under. A category-colored strip above the pane always tells you where you are.
- **Everything persists.** Tabs, order, categories, titles, working directories, and scrollback (colors intact) survive app restarts and reboots. Shells respawn lazily on first focus, with restored scrollback above a `── restored ──` divider.
- **Live Claude states.** Braille spinner while Claude works, a pulsing amber flag when it *needs you* (permission prompts, questions, agent input) with a desktop notification, a quiet ✓ when it finished in a background tab. Driven by Claude Code hooks (one-click install, non-destructive, reversible) with a zero-config fallback that reads Claude's own live session registry.
- **Conversations resume.** Each tab remembers its Claude session; on restore, Giverny re-runs `claude --resume <id>` in the right directory *and* the right account, guarded against double-resuming a session that's already live elsewhere.
- **Multi-account, at a glance.** Profiles are `CLAUDE_CONFIG_DIR`s (auto-discovered, including the `CCTOP_CONFIG_DIRS` convention). Assign an account per category; every tab shows its account. The rail's bottom panel shows each account's limit bars — 5-hour window, weekly, and per-model buckets (e.g. *Fable*) — with reset countdowns and severity colors. Installing hooks also adds a compact Claude statusline that pushes live usage back to Giverny (official `rate_limits` data, no API calls); values it refreshes are marked with a dot, since Claude's own on-disk cache can be days stale for idle accounts.
- **`giverny doctor`** prints exactly what the app sees: profiles, per-profile hook and statusline status, usage freshness, and every live Claude session — the first thing to run if states look wrong.

### Privacy & security

Giverny **never contacts Anthropic and never reads credential files**. Usage meters come from `.claude.json` — a cache Claude Code itself writes; session states come from hook events and Claude Code's local session registry. The only network request Giverny can make is the daily update check against GitHub's releases API, which sends nothing but a User-Agent and can be switched off; with `[update] check = false` it makes no requests at all. Hook installation edits `settings.json` only after showing what it adds, keeps your existing hooks, and writes a `.giverny-bak` backup. Pasted text is stripped of raw escape bytes before it reaches the PTY, and Giverny's in-band control channel is nonce-authenticated so terminal output can't forge Claude states.

## Install

**Linux / macOS**

```sh
curl -fsSL https://github.com/y0av/giverny/releases/latest/download/install.sh | sh
```

**Windows**

```powershell
irm https://github.com/y0av/giverny/releases/latest/download/install.ps1 | iex
```

**From source** (Rust 1.90+):

```sh
git clone https://github.com/y0av/giverny && cd giverny
cargo install --path crates/app
giverny install-desktop     # launcher entry + icons (Linux)
```

Giverny checks GitHub once a day for a newer release and offers a one-click update in the rail; clicking it opens a tab and runs the install command above, so you watch exactly what happens. `giverny update` does the same from a shell, and `[update] check = false` in the config (or `GIVERNY_NO_UPDATE=1`) turns it off entirely.

Giverny also rewrites its own hook paths in `settings.json` whenever you move or reinstall the binary, so switching between a source build and an installed one keeps working.

Linux (Wayland/X11) is tier 1 and daily-driven. macOS and Windows compile in CI and their platform-specific paths are implemented (ConPTY, WSL/PowerShell shell resolution, a spool-file hook relay where unix sockets don't exist) but neither has had a real pass on hardware — reports welcome.

Tagged releases upload prebuilt binaries via GitHub Actions.

## Keys

| Chord | Action |
|---|---|
| `Ctrl+Shift+T` / `Ctrl+Shift+W` | new tab (active category) / close tab |
| `Ctrl+Shift+A` | jump to the next tab where Claude needs you |
| `Ctrl+Shift+P` | fuzzy tab palette |
| `Ctrl+Shift+F` | search scrollback (Enter / Shift+Enter to step) |
| `Ctrl`+hover / click | underline and open a path or URL |
| `Ctrl+PageUp` / `Ctrl+PageDown` | previous / next tab |
| `F2` or double-click | rename tab |
| `Ctrl` `+` / `−` / `0` | font size (persists) |
| `Ctrl+Shift+C` | copy selection |
| middle-click on a tab | close it |
| right-click tab / category | full menu (move, color, account, …) |

## Linux desktop integration

```sh
giverny install-desktop      # --remove to undo
```

Installs `giverny.desktop` and the icon set under `~/.local/share` (the one-line installer does this for you). **On Wayland this is what puts the icon in the dock** — Wayland gives a client no way to hand the compositor its own icon, so the shell matches the window's `app_id` to an installed desktop entry instead. X11 and Windows use the icon compiled into the binary and need no setup. The entry records the binary's current path; re-run it if you move or rebuild elsewhere. `giverny doctor` reports whether it is in place.

## Configuration

`~/.config/giverny/config.toml` is written with comments on first run and hot-reloads on save: font family and size, theme (`monet-dark`, `monet-light`, `ink`), whether restored tabs re-run `claude --resume`, notifications, scrollback depth, and extra account directories. A broken file is reported and ignored rather than blocking startup.

## Status

Early but daily-drivable, and built against Claude Code 2.1.220. Not yet done: split panes, SSH/remote tabs, a plugin system, and hardware passes on macOS/Windows. See [`docs/architecture.md`](docs/architecture.md) for how the terminal engine and Claude integration fit together, [`docs/claude-integration.md`](docs/claude-integration.md) for exactly which Claude Code files are touched, and [`CHANGELOG.md`](CHANGELOG.md) for what landed.

## License

MIT OR Apache-2.0, at your option.
