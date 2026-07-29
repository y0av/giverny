# Options

<!-- Generated from crates/core/src/settings.rs.
     Regenerate: cargo run -p giverny-core --example options -->

Everything in `~/.config/giverny/config.toml`, and everything in the settings screen (`Ctrl+,`) — they are the same list.

| Key | Default | What it does |
|---|---|---|
| `font.family` | `""` | Preferred monospace family; empty auto-detects. |
| `font.size` | `13.0` | Point size of the terminal grid. |
| `theme.name` | `"monet-dark"` | Colour theme for the grid and the chrome around it. One of: monet-dark, monet-light, ink, tokyo-night, gruvbox, nord, catppuccin. |
| `titles.strip_host_prefix` | `true` | Drop the `user@host:` your shell puts in front of every title. |
| `titles.shorten_paths` | `false` | Abbreviate every directory but the last: ~/Dev/bobo becomes ~/D/bobo. |
| `behavior.scrollback_lines` | `10000` | Lines kept above the screen, per tab. |
| `behavior.notifications` | `true` | Notify when Claude needs you in a background tab. |
| `behavior.restore_claude` | `"auto"` | Re-run `claude --resume` in restored tabs. One of: auto, prompt, off. |
| `behavior.restore_apps` | 26 programs | Full-screen programs a restored tab may start again by itself. |
| `behavior.extra_profile_dirs` | `[]` | More CLAUDE_CONFIG_DIRs to show as accounts. |
| `usage.refresh_minutes` | `10` | Ask Claude Code to refresh an account once its numbers are this old. 0 never asks. |
| `update.check` | `true` | Ask GitHub once a day whether a newer Giverny exists. |
