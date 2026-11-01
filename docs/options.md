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
| `rail.place_titles` | `false` | Title a Claude tab after its session's name, as a place: Rotem's Workshop. |
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
| `claude.management_panel` | `true` | Show the tab's subagents — running, planned and done — in a table under the terminal. |
| `claude.resume_after_limit` | `false` | Pick a session back up when the usage window that stopped it reopens. |
| `usage.refresh_minutes` | `10` | Ask Claude Code to refresh an account once its numbers are this old. 0 never asks. |
| `management_panel.manage_skill` | `true` | Ship the /giverny:manage skill with the plugin the management panel installs. |
| `management_panel.ask_names` | `false` | Give every Claude session in a Giverny tab a name at its start; the full name shows on its status line. |
| `management_panel.columns.stage` | `true` | STAGE: Running, Next up or Done. |
| `management_panel.columns.id` | `true` | The task's id (a feed row's key, a subagent's name). |
| `management_panel.columns.title` | `true` | The task's title, or a subagent's description: the column that takes the room left. |
| `management_panel.columns.elapsed` | `true` | How long the row has worked: a stopwatch while it runs. |
| `management_panel.columns.eta` | `true` | Time left on a Running row, the estimate of a Next up one, how late or early a Done one landed. |
| `management_panel.columns.now` | `true` | What the row is doing now: its last tool call, a wait, a queue place. |
| `management_panel.columns.tokens` | `true` | The tokens the row has used. |
| `management_panel.columns.usage` | `true` | What the row uses: on a Running row, live CPU and memory of its `giverny manage run` commands, else of its worker's own processes; on a Done row, the measured memory peak. |
| `management_panel.lease.ram` | `"3G"` | Memory a task's lease holds when nothing says otherwise. Also each Claude tab's protected memory (`MemoryLow`) in `giverny-claude.slice`. A lease reserves RAM only: CPU is shared and measured. |
| `manager.limits.cpu_cores` | `"auto"` | The hard CPU ceiling of every Claude tab and `manage run` together (`giverny-claude.slice`; on macOS held by Giverny itself, a duty cycle on the busy commands). Cores are shared, never leased: a claim waits only while they are measured busy. auto = all but 2. |
| `manager.limits.ram` | `"auto"` | Memory all manager sessions together may hand to workers, and the hard memory ceiling of every Claude tab and `manage run` together (`giverny-claude.slice`; on macOS held by Giverny itself, which kills the biggest command over it). auto = 70 %. |
| `manager.limits.gpus` | `"auto"` | GPUs and VRAM manager sessions may use. auto = 90 % of each GPU's VRAM. |
| `update.check` | `true` | Ask GitHub whether a newer Giverny exists, hourly while it is open. |
