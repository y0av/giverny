# smithay-clipboard (patched)

Upstream [smithay-clipboard](https://github.com/smithay/smithay-clipboard) 0.7.3
(MIT, see `LICENSE`) with one addition: `src/dnd.rs`, and the four
`DataDeviceHandler` methods in `src/state.rs` that upstream leaves empty, so
drag-and-drop reaches the application.

It is applied to the whole build with `[patch.crates-io]` in the workspace root,
which is what makes `eframe`'s clipboard and Giverny's file drops share it.

## Why a fork exists at all

Wayland has one drag-and-drop channel: `wl_data_device`. Mutter keeps **one
per client** — `get_data_device` evicts the client's previous device from its
resource list ([`meta-wayland-data-device.c`][mutter]), so the newest device
receives everything and the older one silently receives nothing.

winit 0.30 — the version egui pins — never creates a data device, so it cannot
deliver drops at all. Creating our own gets the drops but takes the clipboard's
device out of the lists with it, and paste stops working. The one device a
Giverny process owns is the one this crate creates, so drag-and-drop has to be
taught here.

Delete this fork when egui moves to winit 0.31, which delivers Wayland drops
itself.

[mutter]: https://gitlab.gnome.org/GNOME/mutter/-/blob/main/src/wayland/meta-wayland-data-device.c
