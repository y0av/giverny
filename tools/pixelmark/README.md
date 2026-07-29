# pixelmark

**The mark is `marks/giverny.pix`** — a wisteria water lily, eight petals, gold core,
with a one-pixel shadow outline. The outline is what lets a single master survive both
a dark dock and a white README; without it the pale wisteria dissolves on light grounds.
`marks/giverny-tiny.pix` is its 8×8 companion for the rail and statusline, drawn
separately because an outline eats an 8-pixel grid alive.

Everything else in `marks/explore/` is the search that got there — 83 candidates across
six concept families. Kept because the next identity question is cheaper to answer with
them than without.

## Shipping the icon

```sh
python3 make_icons.py
```

Writes `assets/icon/giverny-{16,32,48,64,128,256,512}.png`, `giverny.ico` (Windows),
`giverny.icns` (macOS), `giverny.svg`, and `crates/app/src/icon/mark.rs` — the RGBA
array `icon::icon_data()` upscales for the window icon. Sizes are restricted to integer
multiples of the 16px grid so no pixel ever lands on a fractional boundary; 24 and 96
are deliberately absent. Re-run it after any edit to `giverny.pix`.

---

One hand-authored 16×16 grid is the single source of truth for Giverny's identity —
the window icon, the README art, the mark in the tab rail, and the blob we can print
inside a Giverny pane. No image generator, no vector round-trip, no per-size redraw.

```
marks/*.pix  ──pixelmark.py──┬─→ .png    nearest-neighbour upscale (crisp, never blurred)
                             ├─→ .svg    one <rect> per pixel run
                             ├─→ .ansi   truecolor half-blocks, 2 pixel rows per cell
                             ├─→ .txt    monochrome ASCII, per-palette characters
                             └─→ .rs     RGBA array for eframe::icon_data
```

## Usage

```sh
./preview.sh                       # every mark, in colour, in this terminal
python3 pixelmark.py --sheet       # ./out/*.png + a contact sheet
python3 pixelmark.py marks/2-disc.pix --scale 16 --emit png,rust
```

Useful flags: `--scale` (png upscale, default 16), `--pad` (transparent margin in grid
cells), `--emit` (comma list of `png,svg,ansi,ascii,blocks,rust`), `--show`, `--sheet`.

## The .pix format

```
name: Disc
desc: bloom in a pond ring

palette:
  . = none
  r = #d08aa2      o        # third field: the char used for plain-ASCII output
  G = #f0d178      @

grid:
  .......rr.......
  ......rGGr......
```

`none` means transparent. Colours are `#rrggbb` or `#rrggbbaa`. The parser rejects
ragged grids and undeclared characters, so a miscounted row fails loudly instead of
rendering skewed. Grids need not be 16×16 or square — `7-bloom-tiny.pix` is 8×8.

## Why half-blocks for terminal output

`▀` with a foreground and a background colour carries two vertical pixels, so a 16×16
mark occupies 16 columns × 8 rows with full colour. Density ASCII (`#@.`) throws away
colour and half the vertical resolution; it stays available via `--emit ascii` for
logs, monochrome pipes, and anywhere a code block has to survive copy/paste.

## Colours

Drawn from `CATEGORY_PALETTE` in `crates/app/src/main.rs` — the same Monet hues the
tab rail uses, so the mark reads as part of the UI rather than pasted onto it.
