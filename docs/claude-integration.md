# How Giverny talks to Claude Code

Verified against Claude Code **2.1.220**. Claude Code's internals change between versions; `giverny doctor` is the fastest way to see what the app currently observes.

## What Giverny reads and writes

| Path | Direction | Why |
|---|---|---|
| `<config>/settings.json` | write (on request) | installs the hook relay + usage statusline |
| `<config>/sessions/<pid>.json` | read | live session registry: busy/idle, session id, name |
| `<config>/projects/<munged-cwd>/<id>.jsonl` | read | transcript lookup for resume + session titles |
| `<config>/.claude.json` (or `~/.claude.json`) | read | account identity + `cachedUsageUtilization` |
| `~/.config/giverny/**` | write | Giverny's own state |

`<config>` is each account's `CLAUDE_CONFIG_DIR`. **Credential files are never read, and Giverny makes no network requests.**

## Two state sources, merged

1. **Hooks** — `giverny relay` is registered for `SessionStart`, `UserPromptSubmit`, `Stop`, `Notification`, `SessionEnd`. It forwards the payload plus `$GIVERNY_TAB_ID` and `$CLAUDE_CONFIG_DIR` over a unix socket (a spool file on Windows, or when the app is closed). This gives crisp transitions and the attention taxonomy: `permission_prompt` / `elicitation_dialog` / `agent_needs_input` mean *you* are needed; `idle_prompt` / `agent_completed` / `task_completed` mean done.
2. **Registry** — `sessions/<pid>.json` polled about once a second, PID-liveness gated, mapped to tabs by walking process parents to the tab's shell. Needs no setup and covers sessions that started before hooks were installed.

Authority is **per tab**: the registry drives a tab's state until that tab's session has actually emitted a hook, after which hooks win (the registry file can lag with a stale `busy`). Typing in a tab clears an attention flag — declining a permission prompt with Escape emits no hook at all, so the keystroke is the only evidence.

**Hooks load when a session starts.** After installing, restart `claude` in a tab for hook-driven states.

## Accounts inside WSL (Windows)

On Windows, Claude Code usually lives in a WSL distribution, and `~/.claude` there is a different directory from `~/.claude` on the Windows side. Giverny treats the WSL one as the account it is:

- **Found** by asking `wsl.exe` for its distributions and each one for its home, then reading `\\wsl.localhost\<distro>\home\<user>\.claude` — everything Giverny reads (identity, usage cache, session registry, `settings.json`) is ordinary file IO over that share.
- **Refreshed** by running `claude` *inside* the distribution (`wsl.exe -d <distro> -- env CLAUDE_CONFIG_DIR=… claude -p /usage`), since a Windows `claude` either does not exist or is a different installation.
- **Hooked** by writing a command the distribution can run: this binary, named as `/mnt/c/…/giverny.exe` (asked of `wslpath`, never assumed). Windows programs run from WSL, so the relay executes as a Windows process and writes to the spool the app already watches — no second transport.
- **Identified** across the boundary by `%WSLENV%`, which is what carries `GIVERNY_TAB_ID` into the session and back out with the hook. Without it the relay would see no tab and drop the event.
- **Named** twice on purpose: sessions are told the unix path (`/home/x/.claude`), everything Giverny stores uses the Windows path, and each reported `CLAUDE_CONFIG_DIR` is translated back at the boundary.

A tab whose category names a WSL account opens in that account's distribution. `giverny doctor` prints, per distribution, the `claude` it found and the account directory.

## Resume

Tabs remember `(session id, config dir)`. On restore Giverny finds the transcript on disk (searching every profile, so a lost account association self-heals), reads the conversation's own recorded `cwd` from it, and injects:

```sh
cd "<transcript cwd>" && CLAUDE_CONFIG_DIR="<dir>" command claude --resume <id>
```

`claude --resume` only finds a conversation from the directory it ran in — using the tab's current directory fails once you've `cd`'d. `command` bypasses shell wrapper functions named `claude`. If the session is already live elsewhere, resume is skipped: two writers interleave into one transcript.

## Accounts

An account is a `CLAUDE_CONFIG_DIR`. Giverny finds them, in order: `~/.claude`; `$CLAUDE_CONFIG_DIR`; a shallow scan of `~` and `~/.config` for `claude*` directories that contain an identity file or a session registry; `$CCTOP_CONFIG_DIRS` if you happen to keep one; and `behavior.extra_profile_dirs` from the config.

The last is the general answer for accounts kept anywhere else, and it is what the others feed into: a directory named only by the environment is copied into the config the first time Giverny sees it. Environment variables usually come from a shell rc, so without that step the account list changes depending on whether the app was started from a terminal or from a launcher. `giverny doctor` prints which source each account came from.

## Background agents

`<config>/jobs/<id>/state.json` holds each background agent's state (`working` / `blocked` / `done`), a line of its own description, in-flight task counts, its cwd, and `resumeSessionId` — everything needed to attach a tab. `<config>/daemon/roster.json` lists the workers the daemon is actually running, so a state file that still says `working` after its process died is shown as stale rather than spinning forever. `jobs/pins.json` sorts first.

Parsed defensively out of `serde_json::Value` rather than a derived struct: this is Claude Code's private state and it changes shape. `updatedAt` is an ISO-8601 string where a number looks obvious, and with a derived struct that single mismatch made serde reject the whole record — the agent vanished from the list with no error anywhere.

## Usage meters

Claude Code caches its own usage payload per account in `.claude.json → cachedUsageUtilization`, including the `limits[]` array with the 5-hour window, the weekly window, and model-scoped buckets. Giverny renders `limits[]` only — the legacy scalar keys beside it are placeholder-ridden. A window whose `resets_at` has passed renders 0%: the server keeps the lapsed window's percentage rather than zeroing it.

That cache only refreshes when Claude Code fetches usage, which for idle accounts can be days. The optional statusline (installed with the hooks) pushes the official `rate_limits` fields after every assistant message; those values override the cache when fresher and are marked with a leading `·`.

Three cadences, deliberately different, because the numbers behind them move at different speeds:

| What | How often | Why |
|---|---|---|
| Statusline push | as it happens | Already an event; nothing to poll. |
| Re-read the on-disk caches | 60 s, plus immediately after a refresh completes | The file only changes when Claude fetches. |
| `claude -p /usage` per account | when that account's numbers pass `usage.refresh_minutes` (default 10), and never more than once per window | It spawns a real Claude process. |

The per-account attempt clock is what makes the last row safe: an account with no readable cache is infinitely old, so age alone would spawn a refresh on every tick forever.

Deliberately **not** implemented: polling `api.anthropic.com/api/oauth/usage` (undocumented, unsupported, and inside 2026 ToS language about using subscription OAuth outside Claude Code) and refreshing OAuth tokens (they are single-use and rotating — a third-party refresh logs Claude Code out).
