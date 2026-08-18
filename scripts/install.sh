#!/bin/sh
# Giverny installer for Linux and macOS.
#
#   curl -fsSL https://github.com/y0av/giverny/releases/latest/download/install.sh | sh
#
# Downloads the release binary for this platform, installs it to
# ~/.local/bin (override with GIVERNY_BIN_DIR), and — on Linux — registers
# the desktop entry so the launcher shows an icon.
set -eu

REPO="y0av/giverny"
BIN_DIR="${GIVERNY_BIN_DIR:-$HOME/.local/bin}"
VERSION="${GIVERNY_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "this installer needs $1"; }
need uname
need tar

case "$(uname -s)" in
  Linux)  os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported OS $(uname -s). Build from source: cargo install --path crates/app" ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

target="${arch}-${os}"
asset="giverny-${target}.tar.gz"
if [ "$VERSION" = latest ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading ${asset}"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/$asset" || die "no ${target} build in this release. Build from source instead:
  git clone https://github.com/${REPO} && cd giverny && cargo install --path crates/app"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/$asset" "$url" || die "no ${target} build in this release. Build from source instead:
  git clone https://github.com/${REPO} && cd giverny && cargo install --path crates/app"
else
  die "this installer needs curl or wget"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/giverny" ] || die "archive did not contain a giverny binary"

mkdir -p "$BIN_DIR"
# Replace via a temp file + mv: an atomic rename works even if the old
# binary is currently running.
mv "$tmp/giverny" "$BIN_DIR/giverny.new"
chmod +x "$BIN_DIR/giverny.new"
mv -f "$BIN_DIR/giverny.new" "$BIN_DIR/giverny"
say "installed $BIN_DIR/giverny"

if [ "$os" = unknown-linux-gnu ]; then
  "$BIN_DIR/giverny" install-desktop >/dev/null 2>&1 && say "registered the desktop entry, icons and OOM policy" || true
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    say ""
    say "$BIN_DIR is not on your PATH. Add this to your shell profile:"
    say "  export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac

say ""
say "run: giverny        (and 'giverny doctor' if Claude states look wrong)"
