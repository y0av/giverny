# Contributing

Thanks for looking. Giverny is young; issues describing what broke on your machine are as valuable as patches.

## Building

```sh
cargo run --release        # the app
cargo run -- doctor        # diagnose Claude Code and Codex integration
```

Rust 1.90+. On Debian/Ubuntu you'll want `libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`.

### Running a second instance beside your own

Every Giverny adopts the Claude accounts it finds: it points each account's hooks and status lines at *its own* binary and applies *its own* `management_panel` setting to the `giverny` plugin — even with a separate `XDG_CONFIG_HOME` / `XDG_RUNTIME_DIR`. A test build started beside your everyday Giverny would re-point that Giverny's sessions at the test binary. Start test builds with the account left alone:

```sh
GIVERNY_NO_ACCOUNT_SETUP=1 XDG_CONFIG_HOME=/tmp/gv-config XDG_RUNTIME_DIR=/tmp/gv-run cargo run
```

With it set, Giverny still reads every account but writes nothing into its `settings.json` or `plugins/known_marketplaces.json`, at startup or from the UI (install hooks, the statusline and auto-mode toggles, the management panel switch), and leaves the `giverny` link and the plugin directory where the installed Giverny put them. `GIVERNY_NO_ACCOUNT_SETUP=0` (or empty) is off.

## Before opening a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly these on Linux. On macOS and Windows it builds and runs the same clippy, without tests.

## Where things live

`crates/term` is the terminal engine (PTY, the forked event loop and its byte tee, rendering, input). `crates/core` is the workspace model, config and persistence. `crates/claude` is everything that touches Claude Code's local state. `crates/codex` owns the Codex hook installer and app-server protocol. `crates/app` is the eframe shell and rail. `docs/architecture.md` explains why the event loop is forked and how the two Claude state sources are merged — read it before changing either.

## House rules

- **Never touch credentials or call Anthropic APIs.** Usage and session state come from files Claude Code writes locally. This is a promise to users, not a preference.
- **Keep Codex authentication in Codex.** Use documented app-server RPCs; never read credential files, refresh tokens directly, or start model turns to fetch usage. Use fake helpers in tests so CI needs neither Codex nor credentials.
- **Modify `settings.json` and Codex `hooks.json` non-destructively**: preserve existing hooks, back up first, stay idempotent, and never replace a statusline the user wrote.
- Prefer a test that pins the behavior over a comment explaining it. Terminal and integration bugs here have consistently been things nobody thought to assert.
