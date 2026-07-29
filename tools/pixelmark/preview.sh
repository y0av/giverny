#!/usr/bin/env sh
# Print every mark in this terminal as truecolor half-blocks (2 pixels per cell).
# Needs a truecolor terminal — Giverny itself qualifies, as does most of the field.
cd "$(dirname "$0")" || exit 1
exec python3 pixelmark.py --show --emit "" "$@"
