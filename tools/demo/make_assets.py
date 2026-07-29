#!/usr/bin/env python3
"""Turn captured PPM frames into README assets (still + GIF)."""
import sys
from pathlib import Path

from PIL import Image

src = Path(sys.argv[1] if len(sys.argv) > 1 else "frames")
out = Path(sys.argv[2] if len(sys.argv) > 2 else "assets")
out.mkdir(parents=True, exist_ok=True)

imgs = [Image.open(f).convert("RGB") for f in sorted(src.glob("frame-*.ppm"))]
if not imgs:
    sys.exit(f"no frames in {src}")
w, h = imgs[0].size
small = [im.resize((w // 2, h // 2), Image.LANCZOS) for im in imgs]


def has_flag(im):
    """Frames where the amber attention flag is drawn in the rail."""
    rail = im.crop((0, 0, 30, 150)).getcolors(20000) or []
    return any(c[1][0] > 190 and 150 < c[1][1] < 200 and c[1][2] < 130 for c in rail)


flagged = [i for i, im in enumerate(imgs) if has_flag(im)]
hero = flagged[len(flagged) // 2] if flagged else len(imgs) - 1
small[hero].save(out / "screenshot.png", optimize=True)

# One palette for the whole clip, sampled across it, or quantisation shifts
# the state colours (the critical-usage red turns pink).
sample = small[::6]
montage = Image.new("RGB", (small[0].width, small[0].height * len(sample)))
for i, im in enumerate(sample):
    montage.paste(im, (0, i * small[0].height))
palette = montage.quantize(colors=255, method=Image.MAXCOVERAGE)

frames = [im.quantize(palette=palette, dither=Image.NONE) for im in small]
durations = [110] * len(frames)
durations[-1] = 1800  # hold on the final state
frames[0].save(
    out / "demo.gif", save_all=True, append_images=frames[1:],
    duration=durations, loop=0, optimize=True,
)
for name in ("screenshot.png", "demo.gif"):
    print(name, (out / name).stat().st_size // 1024, "KB")
