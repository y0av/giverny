# Demo capture

Regenerates `assets/screenshot.png` and `assets/demo.gif`.

**Everything shown is invented.** The projects, accounts, usage numbers and
Claude transcripts are mock data staged in `/tmp/giverny-demo`; the capture
runs with `HOME` pointed there, so it can neither read nor disturb a real
`~/.config/giverny` or `~/.claude`.

```sh
python3 tools/demo/stage_demo.py      # mock home: accounts, repos, workspace
python3 tools/demo/scenes.py          # the terminal content each tab shows
tools/demo/drive_demo.sh &            # feeds mock Claude states while capturing
HOME=/tmp/giverny-demo \
  XDG_RUNTIME_DIR=/tmp/giverny-demo-run \
  WAYLAND_DISPLAY=/run/user/$(id -u)/wayland-0 \
  SHELL=/tmp/giverny-demo/demo-shell.sh \
  CCTOP_CONFIG_DIRS=/tmp/giverny-demo/.claude:/tmp/giverny-demo/envs/lab/claude:/tmp/giverny-demo/envs/night/claude \
  GIVERNY_NO_UPDATE=1 GIVERNY_CAPTURE=frames:48:11 \
  ./target/release/giverny
python3 tools/demo/make_assets.py frames assets
```

## How the capture works

`GIVERNY_CAPTURE=<dir>[:<frames>[:<stride>]]` makes Giverny photograph its own
framebuffer (`crates/app/src/capture.rs`) and exit when done. Wayland offers no
way to screenshot a window from outside without a portal prompt, and this also
guarantees exact window pixels rather than whatever the compositor composites.
Frames are binary PPM so the app needs no image-encoding dependency; the Python
step converts them.

`XDG_RUNTIME_DIR` is redirected so the demo binds its own hook socket instead of
taking over a real instance's, and `WAYLAND_DISPLAY` is given as an absolute path
so the compositor is still reachable.
