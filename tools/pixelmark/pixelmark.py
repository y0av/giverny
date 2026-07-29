#!/usr/bin/env python3
"""pixelmark — render hand-authored pixel grids to icons, terminal art, SVG and Rust.

One 16x16 (or NxM) character grid is the single source of truth for the whole
identity: the window icon, the README art, the splash mark in the tab rail, and
the ANSI blob we can print inside a Giverny pane.

A .pix file looks like:

    name: Bloom
    desc: top-down water lily
    palette:
      . = none
      d = #a85f78   o      # third field is the char used for plain-ASCII output
      r = #d08aa2   *
    grid:
      .......dd.......
      ......drrd......

Outputs:
  png    nearest-neighbour upscale (crisp pixels, never blurred)
  ansi   truecolor half-block art  — 2 pixel rows per terminal row
  ascii  monochrome, using each palette entry's ascii char
  svg    one <rect> per run of pixels
  rust   RGBA byte array for eframe::icon_data / egui ColorImage
  sheet  contact sheet PNG of every mark at several sizes
"""

from __future__ import annotations

import argparse
import struct
import sys
import zlib
from dataclasses import dataclass, field
from pathlib import Path

TRANSPARENT = (0, 0, 0, 0)


# ---------------------------------------------------------------- parsing


@dataclass
class Mark:
    name: str
    desc: str
    path: Path
    colors: dict[str, tuple[int, int, int, int]] = field(default_factory=dict)
    ascii_chars: dict[str, str] = field(default_factory=dict)
    rows: list[str] = field(default_factory=list)

    @property
    def width(self) -> int:
        return len(self.rows[0]) if self.rows else 0

    @property
    def height(self) -> int:
        return len(self.rows)

    @property
    def slug(self) -> str:
        return self.path.stem

    def rgba(self, x: int, y: int) -> tuple[int, int, int, int]:
        return self.colors.get(self.rows[y][x], TRANSPARENT)


def parse_color(text: str) -> tuple[int, int, int, int]:
    text = text.strip()
    if text.lower() in ("none", "-", "transparent"):
        return TRANSPARENT
    if not text.startswith("#"):
        raise ValueError(f"colour must be #rrggbb[aa] or 'none', got {text!r}")
    digits = text[1:]
    if len(digits) == 6:
        digits += "ff"
    if len(digits) != 8:
        raise ValueError(f"colour must be #rrggbb or #rrggbbaa, got {text!r}")
    return tuple(int(digits[i : i + 2], 16) for i in (0, 2, 4, 6))  # type: ignore[return-value]


def load(path: Path) -> Mark:
    mark = Mark(name=path.stem, desc="", path=path)
    section = None
    for lineno, raw in enumerate(path.read_text().splitlines(), 1):
        line = raw.rstrip()
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        head = line.strip()
        if head.endswith(":") and head[:-1] in ("palette", "grid"):
            section = head[:-1]
            continue
        if section is None:
            key, _, value = head.partition(":")
            if key == "name":
                mark.name = value.strip()
            elif key == "desc":
                mark.desc = value.strip()
            else:
                raise ValueError(f"{path}:{lineno}: unknown header {key!r}")
        elif section == "palette":
            symbol, sep, rest = head.partition("=")
            if not sep:
                raise ValueError(f"{path}:{lineno}: expected '<char> = <colour>'")
            symbol = symbol.strip()
            if len(symbol) != 1:
                raise ValueError(f"{path}:{lineno}: palette key must be one char")
            parts = rest.split()
            mark.colors[symbol] = parse_color(parts[0])
            mark.ascii_chars[symbol] = parts[1] if len(parts) > 1 else (
                " " if mark.colors[symbol][3] == 0 else "#"
            )
        elif section == "grid":
            mark.rows.append(head)

    if not mark.rows:
        raise ValueError(f"{path}: no grid rows")
    widths = {len(r) for r in mark.rows}
    if len(widths) != 1:
        detail = ", ".join(f"row {i}={len(r)}" for i, r in enumerate(mark.rows))
        raise ValueError(f"{path}: ragged grid ({detail})")
    unknown = {c for r in mark.rows for c in r} - set(mark.colors)
    if unknown:
        raise ValueError(f"{path}: grid uses undeclared chars {sorted(unknown)}")
    return mark


# ---------------------------------------------------------------- png writer


def write_png(path: Path, width: int, height: int, pixels: list[list[tuple[int, int, int, int]]]) -> None:
    """Minimal RGBA PNG encoder — no dependency on Pillow for the core path."""
    raw = bytearray()
    for row in pixels:
        raw.append(0)  # filter: none
        for r, g, b, a in row:
            raw += bytes((r, g, b, a))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def upscale(mark: Mark, scale: int, pad: int = 0, bg: tuple[int, int, int, int] = TRANSPARENT):
    w, h = mark.width + pad * 2, mark.height + pad * 2
    out = []
    for y in range(h * scale):
        row = []
        gy = y // scale - pad
        for x in range(w * scale):
            gx = x // scale - pad
            if 0 <= gx < mark.width and 0 <= gy < mark.height:
                px = mark.rgba(gx, gy)
                row.append(px if px[3] else bg)
            else:
                row.append(bg)
        out.append(row)
    return w * scale, h * scale, out


# ---------------------------------------------------------------- renderers


def render_ansi(mark: Mark, indent: str = "") -> str:
    """Truecolor half-block art: '▀' carries the upper pixel as fg, lower as bg."""
    lines = []
    for y in range(0, mark.height, 2):
        buf = [indent]
        for x in range(mark.width):
            top = mark.rgba(x, y)
            bot = mark.rgba(x, y + 1) if y + 1 < mark.height else TRANSPARENT
            if top[3] and bot[3]:
                buf.append(f"\x1b[38;2;{top[0]};{top[1]};{top[2]}m\x1b[48;2;{bot[0]};{bot[1]};{bot[2]}m▀")
            elif top[3]:
                buf.append(f"\x1b[49m\x1b[38;2;{top[0]};{top[1]};{top[2]}m▀")
            elif bot[3]:
                buf.append(f"\x1b[49m\x1b[38;2;{bot[0]};{bot[1]};{bot[2]}m▄")
            else:
                buf.append("\x1b[0m ")
        buf.append("\x1b[0m")
        lines.append("".join(buf))
    return "\n".join(lines)


def render_ascii(mark: Mark, indent: str = "") -> str:
    return "\n".join(
        indent + "".join(mark.ascii_chars[c] for c in row).rstrip() for row in mark.rows
    )


def render_blocks(mark: Mark, indent: str = "") -> str:
    """Colourless half-blocks — a silhouette that survives copy/paste anywhere."""
    glyphs = {(1, 1): "█", (1, 0): "▀", (0, 1): "▄", (0, 0): " "}
    lines = []
    for y in range(0, mark.height, 2):
        buf = []
        for x in range(mark.width):
            top = 1 if mark.rgba(x, y)[3] else 0
            bot = 1 if (y + 1 < mark.height and mark.rgba(x, y + 1)[3]) else 0
            buf.append(glyphs[(top, bot)])
        lines.append(indent + "".join(buf).rstrip())
    return "\n".join(lines)


def render_svg(mark: Mark) -> str:
    w, h = mark.width, mark.height
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" '
        f'width="{w * 16}" height="{h * 16}" shape-rendering="crispEdges">'
    ]
    for y, row in enumerate(mark.rows):
        x = 0
        while x < w:
            px = mark.rgba(x, y)
            if px[3] == 0:
                x += 1
                continue
            run = 1
            while x + run < w and mark.rgba(x + run, y) == px:
                run += 1
            hexcol = "#%02x%02x%02x" % px[:3]
            alpha = "" if px[3] == 255 else f' fill-opacity="{px[3] / 255:.3f}"'
            parts.append(f'<rect x="{x}" y="{y}" width="{run}" height="1" fill="{hexcol}"{alpha}/>')
            x += run
    parts.append("</svg>")
    return "\n".join(parts)


def render_rust(mark: Mark) -> str:
    ident = mark.slug.upper().replace("-", "_")
    body = []
    for y in range(mark.height):
        cells = []
        for x in range(mark.width):
            r, g, b, a = mark.rgba(x, y)
            cells.append(f"{r},{g},{b},{a},")
        body.append("    " + "".join(cells))
    return (
        f"// generated by tools/pixelmark — edit {mark.path.name}, not this file\n"
        f"pub const {ident}_W: u32 = {mark.width};\n"
        f"pub const {ident}_H: u32 = {mark.height};\n"
        f"pub const {ident}_RGBA: [u8; {mark.width * mark.height * 4}] = [\n"
        + "\n".join(body)
        + "\n];\n"
    )


# ---------------------------------------------------------------- contact sheet


def contact_sheet(marks: list[Mark], path: Path, sizes=(16, 32, 64, 128)) -> None:
    """Every mark, one row each, rendered at several pixel sizes on a dark pane."""
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError:
        print("contact sheet needs Pillow; skipping", file=sys.stderr)
        return

    bg = (16, 22, 28)
    label_w, gap, pad = 150, 22, 24
    row_h = max(sizes) + 34
    sheet_w = label_w + sum(s + gap for s in sizes) + pad * 2 + 80
    sheet_h = pad * 2 + row_h * len(marks) + 40

    img = Image.new("RGB", (sheet_w, sheet_h), bg)
    draw = ImageDraw.Draw(img)

    def font(size):
        for candidate in (
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ):
            if Path(candidate).exists():
                return ImageFont.truetype(candidate, size)
        return ImageFont.load_default()

    f_title, f_label, f_small = font(17), font(15), font(11)
    draw.text((pad, pad - 4), "Giverny — icon candidates", fill=(215, 221, 226), font=f_title)

    y = pad + 34
    for mark in marks:
        draw.text((pad, y + 6), mark.name, fill=(215, 221, 226), font=f_label)
        draw.text((pad, y + 26), mark.slug, fill=(107, 120, 128), font=f_small)
        wrapped = mark.desc
        draw.text((pad, y + 42), wrapped[:24], fill=(107, 120, 128), font=f_small)

        x = pad + label_w
        for size in sizes:
            scale = max(1, size // max(mark.width, mark.height))
            w, h, px = upscale(mark, scale)
            tile = Image.new("RGBA", (w, h))
            tile.putdata([p for row in px for p in row])
            img.paste(tile, (x, y + (max(sizes) - h) // 2), tile)
            draw.text((x, y + max(sizes) + 6), f"{size}px", fill=(107, 120, 128), font=f_small)
            x += size + gap
        y += row_h

    img.save(path)


# ---------------------------------------------------------------- cli


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("marks", nargs="*", type=Path, help=".pix files (default: ./marks/*.pix)")
    ap.add_argument("-o", "--out", type=Path, default=Path("out"), help="output directory")
    ap.add_argument("--scale", type=int, default=16, help="png upscale factor")
    ap.add_argument("--pad", type=int, default=0, help="transparent padding in grid cells")
    ap.add_argument(
        "--emit",
        default="png,svg,ansi,ascii",
        help="comma list of png,svg,ansi,ascii,blocks,rust",
    )
    ap.add_argument("--sheet", action="store_true", help="also write contact-sheet.png")
    ap.add_argument("--show", action="store_true", help="print ANSI art to stdout")
    args = ap.parse_args()

    paths = args.marks or sorted((Path(__file__).parent / "marks").glob("*.pix"))
    if not paths:
        print("no .pix files found", file=sys.stderr)
        return 1

    marks = []
    for p in paths:
        try:
            marks.append(load(p))
        except ValueError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 1

    emit = {e.strip() for e in args.emit.split(",") if e.strip()}
    args.out.mkdir(parents=True, exist_ok=True)

    for mark in marks:
        stem = args.out / mark.slug
        if "png" in emit:
            w, h, px = upscale(mark, args.scale, args.pad)
            write_png(stem.with_suffix(".png"), w, h, px)
        if "svg" in emit:
            stem.with_suffix(".svg").write_text(render_svg(mark))
        if "ansi" in emit:
            stem.with_suffix(".ansi").write_text(render_ansi(mark) + "\n")
        if "ascii" in emit:
            stem.with_suffix(".txt").write_text(render_ascii(mark) + "\n")
        if "blocks" in emit:
            stem.with_name(mark.slug + ".blocks.txt").write_text(render_blocks(mark) + "\n")
        if "rust" in emit:
            stem.with_suffix(".rs").write_text(render_rust(mark))
        if args.show:
            print(f"\n\x1b[1m{mark.name}\x1b[0m \x1b[2m— {mark.desc}\x1b[0m")
            print(render_ansi(mark, indent="  "))

    if args.sheet:
        contact_sheet(marks, args.out / "contact-sheet.png")

    print(f"{len(marks)} mark(s) → {args.out}/", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
