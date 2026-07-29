# Contributing

Thanks for looking. Giverny is young; issues describing what broke on your machine are as valuable as patches.

## Building

```sh
cargo run --release        # the app
cargo run -- doctor        # diagnose Claude Code integration
```

Rust 1.90+. On Debian/Ubuntu you'll want `libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`.

## Before opening a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly these on Linux, plus build-only jobs for macOS and Windows.

## Where things live

`crates/term` is the terminal engine (PTY, the forked event loop and its byte tee, rendering, input). `crates/core` is the workspace model, config and persistence. `crates/claude` is everything that touches Claude Code's local state. `crates/app` is the eframe shell and rail. `docs/architecture.md` explains why the event loop is forked and how the two Claude state sources are merged — read it before changing either.

## House rules

- **Never touch credentials or call Anthropic APIs.** Usage and session state come from files Claude Code writes locally. This is a promise to users, not a preference.
- **Modify `settings.json` non-destructively**: preserve existing hooks, back up first, stay idempotent, and never replace a statusline the user wrote.
- Prefer a test that pins the behavior over a comment explaining it. Terminal and integration bugs here have consistently been things nobody thought to assert.
