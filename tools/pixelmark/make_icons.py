#!/usr/bin/env python3
"""Build every shipping form of the mark from marks/giverny.pix.

    assets/icon/giverny-<n>.png   16 … 512, integer nearest-neighbour upscales
    assets/icon/giverny.ico       Windows, 16–256 in one container
    assets/icon/giverny.icns      macOS, PNG-backed types incl. @2x
    assets/icon/giverny.svg       one <rect> per pixel run
    crates/app/src/icon/mark.rs   the 16×16 RGBA array the window icon is built from

Sizes are restricted to integer multiples of the 16px grid so a pixel never
lands on a fractional boundary — 24px and 96px are deliberately absent.
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import pixelmark as pm

ROOT = Path(__file__).resolve().parents[2]
MARK = Path(__file__).parent / "marks" / "giverny.pix"
ICON_DIR = ROOT / "assets" / "icon"
RUST_OUT = ROOT / "crates" / "app" / "src" / "icon" / "mark.rs"

PNG_SIZES = [16, 32, 48, 64, 128, 256, 512]
ICO_SIZES = [16, 32, 48, 64, 128, 256]
ICNS_TYPES = [(b"icp4", 16), (b"icp5", 32), (b"ic11", 32), (b"ic12", 64),
              (b"ic07", 128), (b"ic08", 256), (b"ic13", 256), (b"ic09", 512), (b"ic14", 512)]


def png_bytes(mark: pm.Mark, size: int) -> bytes:
    scale, rem = divmod(size, max(mark.width, mark.height))
    if rem:
        raise SystemExit(f"{size}px is not an integer multiple of the {mark.width}px grid")
    w, h, px = pm.upscale(mark, scale)
    tmp = ICON_DIR / f".tmp-{size}.png"
    pm.write_png(tmp, w, h, px)
    data = tmp.read_bytes()
    tmp.unlink()
    return data


def write_icns(path: Path, mark: pm.Mark) -> None:
    chunks = b""
    cache: dict[int, bytes] = {}
    for tag, size in ICNS_TYPES:
        blob = cache.setdefault(size, png_bytes(mark, size))
        chunks += tag + struct.pack(">I", len(blob) + 8) + blob
    path.write_bytes(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def write_ico(path: Path, mark: pm.Mark) -> None:
    """ICO is a directory of PNGs; PIL's encoder re-samples, so assemble it directly."""
    entries, blobs, offset = b"", b"", 6 + 16 * len(ICO_SIZES)
    for size in ICO_SIZES:
        blob = png_bytes(mark, size)
        entries += struct.pack(
            "<BBBBHHII", 0 if size >= 256 else size, 0 if size >= 256 else size,
            0, 0, 1, 32, len(blob), offset)
        blobs += blob
        offset += len(blob)
    path.write_bytes(struct.pack("<HHH", 0, 1, len(ICO_SIZES)) + entries + blobs)


def main() -> int:
    mark = pm.load(MARK)
    ICON_DIR.mkdir(parents=True, exist_ok=True)
    RUST_OUT.parent.mkdir(parents=True, exist_ok=True)

    for size in PNG_SIZES:
        (ICON_DIR / f"giverny-{size}.png").write_bytes(png_bytes(mark, size))
    (ICON_DIR / "giverny.svg").write_text(pm.render_svg(mark))
    write_ico(ICON_DIR / "giverny.ico", mark)
    write_icns(ICON_DIR / "giverny.icns", mark)
    RUST_OUT.write_text(pm.render_rust(mark))

    print(f"png   {', '.join(str(s) for s in PNG_SIZES)}")
    print(f"ico   {ICON_DIR / 'giverny.ico'}")
    print(f"icns  {ICON_DIR / 'giverny.icns'}")
    print(f"svg   {ICON_DIR / 'giverny.svg'}")
    print(f"rust  {RUST_OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
