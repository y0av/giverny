# Settings — design

Status: built, except where §9 says otherwise. Kept as the record of why it is
shaped this way.

Giverny has no settings screen. Preferences live in `config.toml` (hot-reloaded),
object properties live in right-click menus, and a few things — font size, rail
width, window size — are only reachable by doing them. That is a defensible
place to stop, and it is where Ghostty deliberately stops. We are not stopping
there, because the app now has enough knobs that "read the docs and edit TOML"
is the only way to discover most of them.

The whole risk of adding one is that it turns a terminal into a preferences app.
Everything below is arranged around not doing that.

## 1. Who owns the truth

Three stores exist today and the distinction has to survive:

| Store | Holds | Edited by |
|---|---|---|
| `config.toml` | preferences — font, theme, behavior, usage, update | hand + (new) settings screen |
| `state/tabs.json` | the workspace — tabs, categories, layout, font size | the app only |
| workspace objects | category color, category account, tab title | right-click menus |

**The settings screen edits `config.toml` and nothing else.** Category colors
and per-tab settings stay in context menus: they are properties of a thing you
are pointing at, not application preferences, and moving them into a settings
screen makes both worse.

### The file stays hand-editable

This is the Windows Terminal problem and they got it right after getting it
wrong: the settings UI is a *view over the file*, not a replacement for it. Their
implementation note is the one that matters — "layer the output JSON onto the
user's existing settings; we need to make sure we don't overwrite any comments"
([microsoft/terminal#1564](https://github.com/microsoft/terminal/issues/1564)).

Our config template ships as commented TOML. A naive `toml::to_string(&config)`
write-back destroys every comment in it, including the user's. So:

- Write with **`toml_edit`** (already in the lockfile via `toml`), which preserves
  comments, ordering and formatting.
- Write **only the keys that changed**, never the whole document.
- Atomic tmp+rename, as everywhere else.
- Our own config watcher then reloads it. The write is idempotent, so the round
  trip is a no-op.

kitty solves the same problem differently — its theme picker writes a separate
`current-theme.conf` and `include`s it, commenting out conflicting settings in
the main file. Elegant, but it needs an include mechanism we do not have and
splits the truth across two files. `toml_edit` is the smaller answer.

### One wart to fix on the way

`font.size` exists in `config.toml` *and* `font_size` in `tabs.json` (Ctrl+± writes
the latter, which then wins). Two sources of truth for one value. Ctrl+± should
write `config.toml` through the same path the settings screen uses, and
`tabs.json` should drop the field.

## 2. Form factor

**Decided: a full-height overlay over the terminal pane, rail still visible.**
`Ctrl+,` opens it, `Esc` closes it, the command palette can jump straight to a
section.

Not a tab: a tab is a PTY. A settings tab breaks that equivalence and raises
questions with no good answers (does it persist? does it restore? what is its
scrollback?). Not a separate window: a second window for a single-window app is
a platform problem (Wayland placement, taskbar, focus) bought for nothing.

Keeping the rail visible matters — it is the app's identity, and settings that
change the rail (titles, colors, theme) can be watched taking effect while you
change them.

## 3. Shape

```
┌ settings ────────────────────────────────────────────────────┐
│ / search                                              esc ✕  │
├──────────────┬───────────────────────────────────────────────┤
│ appearance   │  theme                          [monet-dark ▾]│
│ terminal     │  theme.name                                   │
│ tabs & title │                                               │
│ restore    • │  font family                 [JetBrainsMono ▾] │
│ claude       │  font.family                                  │
│ keys         │                                               │
│ updates      │  font size                        [ 13.0 ]  ● │
│ about        │  font.size            default 13.0    ↺ reset │
├──────────────┴───────────────────────────────────────────────┤
│ ↑↓ move · ⏎ edit · / search · ⇧⏎ open config.toml · esc back │
└──────────────────────────────────────────────────────────────┘
```

Four decisions in that sketch:

1. **Every row shows its TOML key** under the human label. The screen teaches the
   file. Anyone who spends a minute in here can then edit `config.toml` over SSH
   or put it in their dotfiles, which is the actual power-user path.
2. **A key hint bar along the bottom**, the oldest TUI convention there is —
   htop, lazygit, k9s, mc. Memory is not a prerequisite for using the app, and
   `Esc` means back.
3. **Search first.** One box, filtering every row in every section by label, key
   and doc text. In a settings screen of any size, search is the navigation.
4. **Modified rows are marked** (`●`) and individually resettable. What you
   changed should be visible without diffing against defaults in your head.

Monospace throughout; toggles render as `[ on ]`/`[ off ]`, not switches. Box
drawing, not cards and shadows.

`⇧⏎` opens `config.toml` in `$EDITOR` **in a new Giverny tab** — the settings
screen handing you a terminal is the most on-brand thing it can do.

## 4. One schema, three outputs

The failure mode for settings screens is rot: a new option lands in the struct,
the screen never learns about it, the docs disagree with both.

So the options are declared once — key path, type, default, one-line doc, section
— and that table generates:

- the settings rows (widget chosen by type),
- the commented `config.toml` template written on first run,
- the options table in the docs.

Search comes free, "reset to default" comes free, and an option that is not in
the table is not in the app.

## 5. Sections

| Section | Rows |
|---|---|
| appearance | theme (live preview), font family, font size, cursor blink |
| terminal | scrollback lines, bell → notification, paste guard |
| tabs & titles | strip `user@host:` prefix, shorten paths, title source order, new-tab directory |
| restore | resume Claude (auto/prompt/off), **restore-apps list**, restore scrollback |
| claude | profiles + extra config dirs, hooks state per profile, statusline toggle, usage refresh interval |
| keys | the full keymap, searchable |
| updates | check on/off, current version, check now |
| about | version, config path, state path, run `doctor` |

`claude` and `about` overlap with what the rail's bottom panel and `giverny
doctor` already show. The settings screen should *link* to those, not restate
them — one place per fact.

## 6. The four asks

### Restore-apps list (edit + add)

A list editor in **restore**. Rows with a remove affordance, plus an add field.

Two things make it better than a text box:

- **Suggest from what you have actually run.** Every tab already records its
  foreground command for restore; the union of those, minus the ones already
  allowed, is a one-click "allow this" list. No typing, no guessing at program
  names.
- **Say what allowing means, once, in the section.** Adding a program means
  Giverny will run it unattended when a tab restores. That is fine for `btop`
  and not fine for `./deploy.sh`. The line stays visible; it is not a modal.

Config semantics: setting `restore_apps` *replaces* the default list. The editor
writes the full effective list, so replace-vs-extend never becomes something a
user has to reason about.

### Keymap view

**Decided: read-only, two views.** One table in code — chord, action, scope (global / terminal / rail), description
— rendered in two places:

- the **keys** section of settings, searchable;
- a **quick overlay** on `F1` (and `Ctrl+Shift+/`), which does not disturb what
  you were doing.

Same data, so they cannot drift. It also feeds chord hints into the command
palette, which currently shows none.

**Rebinding is deliberately not in stage one.** It needs chord capture, conflict
detection, persistence, and — the part that actually bites — a rule for when a
binding may shadow a key the shell or Claude needs. Read-only first; the table
is shaped so rebinding drops in behind it later.

### Strip `user@host:` from titles

Real example from this machine: `yoz@yoz-framework:~/Dev/bobo`. Twenty of the
thirty characters are the same on every tab, in a rail that is 240px wide.

`[titles] strip_host_prefix = true`, default on. Strips a leading
`name@host:` (`[A-Za-z0-9._-]+@[A-Za-z0-9._-]+:\s*`) — narrow enough not to eat
`ssh: user@host` or a title that merely contains an `@`.

The implementation detail that matters: **keep the raw title, transform at
display time.** Toggling then applies instantly to every existing tab instead of
only to titles set after the change.

Its natural sibling, same section: shorten long paths (`~/Dev/bobo` → `~/D/bobo`,
or just the last segment). Same problem — fitting a title into a narrow rail.

### Theming

**Decided: stage 1 only for now.** A picker with live preview — apply on
selection, revert on `Esc` — over the three built-ins plus a handful of the
themes everyone already knows (Tokyo Night, Gruvbox, Nord, Catppuccin). Colour
values, no dependencies.

Stage 2, if it is ever wanted: load `~/.config/giverny/themes/*.toml`, so anyone
can drop in one of the thousands of existing terminal themes. Worth designing the
built-ins as if they were loaded from that format, so stage 2 is a loader and not
a rewrite.

Category colours are theming-adjacent and stay where they are (right-click on a
category). The Monet palette is the app's identity; per-category overrides
already exist.

## 7. Deliberately out

- Rebinding (above).
- Per-profile settings — that is Claude's `settings.json`, not ours.
- Import/export, settings sync. The config file *is* the export.
- Anything that only makes sense once split panes exist.

## 8. Risks

- **It becomes a preferences app.** Mitigated by the aesthetic rules in §3 and by
  §7 — the list of things it will not grow.
- **Two writers.** The user has `config.toml` open in an editor while the screen
  writes it. Unavoidable in principle; atomic writes plus hot-reload make the
  window small and the loser obvious.
- **Schema drift** between the settings table and the config structs. The
  generated template (§4) is the canary: if a key is missing from the template,
  it is missing from the table.

## 9. Build order

1. ✅ The options schema (§4) and `toml_edit` write-back, with the generated
   template replacing the hand-written one.
2. ✅ The overlay shell: sections, search, key hint bar, `Ctrl+,`, `Esc`,
   appearance and terminal rows.
3. ✅ Titles, including strip-prefix and path shortening as display-time
   transforms.
4. ✅ Restore, with the list editor and suggestions from what tabs have run.
5. ✅ Keys: one table, the settings section and the `F1` overlay.
6. ✅ Theme picker and the extra built-ins.
7. ✅ Claude, updates, about.

Still open, deliberately:

- **Live preview on hover.** Themes apply on click, which is instant and
  reversible by clicking back; hovering to preview and reverting on `Esc` would
  be better and is not built.
- **Font family needs a restart.** The row changes the config, but the atlas is
  built once at startup. The screen should say so on that row; today it only
  logs.
- **Palette chord hints.** The keymap table exists to feed them; the palette
  does not read it yet.
- **Rebinding**, per §6.
- Stage 2 theming (`~/.config/giverny/themes/*.toml`), per §6.
