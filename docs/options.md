# Options

<!-- Generated from crates/core/src/settings.rs.
     Regenerate: cargo run -p giverny-core --example options -->

Everything in `~/.config/giverny/config.toml`, and everything in the settings screen (`Ctrl+,`) — they are the same list.

| Key | Default | What it does |
|---|---|---|
| `font.family` | `""` | Preferred monospace family; empty auto-detects. Hebrew comes from the bundled Cascadia Mono unless this family has Hebrew of its own. Takes effect on restart. |
| `font.size` | `13.0` | Point size of the terminal grid. |
| `theme.name` | `"monet-dark"` | Colour theme for the grid and the chrome around it. One of: monet-dark, monet-light, ink, tokyo-night, gruvbox, nord, catppuccin, catppuccin-mauve, rouen, phosphor, abyss, synthwave, workbench, riso, red-rock. |
| `window.opacity` | `1.0` | How solid the window's background is; below 1.0 the desktop shows through. Takes effect on restart. |
| `titles.strip_host_prefix` | `true` | Drop the `user@host:` your shell puts in front of every title. |
| `titles.shorten_paths` | `false` | Abbreviate every directory but the last: ~/Dev/bobo becomes ~/D/bobo. |
| `titles.strip_claude_glyph` | `true` | Drop the glyph Claude Code puts in front of its title (✳, ◐, a spinner). |
| `rail.animate` | `true` | Spin a working tab's mark and pulse a tab that wants you; off removes the spinners and the pulse. |
| `rail.calm_busy` | `true` | A working tab breathes a slow dot in its colour instead of spinning. |
| `rail.tab_figures` | `true` | Each tab's CPU and memory in the rail, and a collapsed group's sum. |
| `rail.total_line` | `true` | The summed CPU and memory line under the account bars. |
| `behavior.scrollback_lines` | `10000` | Lines kept above the screen, per tab. |
| `behavior.history_per_tab` | `false` | Each tab's shell keeps its own history, back when the tab is restored. |
| `behavior.history_also_shared` | `false` | With history per terminal, also append each command to the usual history (bash). |
| `behavior.notifications` | `true` | Notify when an agent needs you, or Codex finishes in a background tab. |
| `behavior.prefer_x11` | `false` | Run under X11/XWayland instead of Wayland. Takes effect on restart. |
| `behavior.restore_claude` | `"auto"` | Re-run `claude --resume` in restored tabs. One of: auto, prompt, off. |
| `behavior.windows_shell` | `"auto"` | Which shell a new tab opens on Windows. One of: auto, wsl, powershell, cmd. |
| `behavior.restore_apps` | 26 programs | Full-screen programs a restored tab may start again by itself. |
| `behavior.extra_profile_dirs` | `[]` | Account directories kept somewhere Giverny would not find on its own. Takes effect on restart. |
| `codex.enabled` | `true` | Show detected Codex accounts and usage automatically. Turn off to disable integration. |
| `codex.executable` | `"codex"` | Executable name on PATH, or an explicit path. |
| `codex.home` | `""` | Empty uses CODEX_HOME, then the default .codex directory. |
| `codex.restore` | `"auto"` | Auto reconnects saved conversations on tab focus; prompt asks first; off skips restore. One of: auto, prompt, off. |
| `codex.usage_refresh_minutes` | `10` | Minutes between Codex usage requests. Zero means manual refresh only. |
| `claude.auto_mode` | `false` | Every new Claude session starts in auto mode instead of asking for each permission. |
| `claude.skip_resume_summary` | `false` | Skip Claude Code's offer to resume from a summary, and resume the full session. |
| `claude.resume_after_limit` | `false` | Pick a session back up when the usage window that stopped it reopens. |
| `usage.refresh_minutes` | `10` | Ask Claude Code to refresh an account once its numbers are this old. 0 never asks. |
| `update.check` | `true` | Ask GitHub whether a newer Giverny exists, hourly while it is open. |
