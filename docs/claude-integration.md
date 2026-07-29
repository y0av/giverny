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

## Resume

Tabs remember `(session id, config dir)`. On restore Giverny finds the transcript on disk (searching every profile, so a lost account association self-heals), reads the conversation's own recorded `cwd` from it, and injects:

```sh
cd "<transcript cwd>" && CLAUDE_CONFIG_DIR="<dir>" command claude --resume <id>
```

`claude --resume` only finds a conversation from the directory it ran in — using the tab's current directory fails once you've `cd`'d. `command` bypasses shell wrapper functions named `claude`. If the session is already live elsewhere, resume is skipped: two writers interleave into one transcript.

## Usage meters

Claude Code caches its own usage payload per account in `.claude.json → cachedUsageUtilization`, including the `limits[]` array with the 5-hour window, the weekly window, and model-scoped buckets. Giverny renders `limits[]` only — the legacy scalar keys beside it are placeholder-ridden. A window whose `resets_at` has passed renders 0%: the server keeps the lapsed window's percentage rather than zeroing it.

That cache only refreshes when Claude Code fetches usage, which for idle accounts can be days. The optional statusline (installed with the hooks) pushes the official `rate_limits` fields after every assistant message; those values override the cache when fresher and are marked with a leading `·`.

Deliberately **not** implemented: polling `api.anthropic.com/api/oauth/usage` (undocumented, unsupported, and inside 2026 ToS language about using subscription OAuth outside Claude Code) and refreshing OAuth tokens (they are single-use and rotating — a third-party refresh logs Claude Code out).
