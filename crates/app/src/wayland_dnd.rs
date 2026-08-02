//! Drag-and-drop on Wayland.
//!
//! winit 0.30 — the version egui pins — has no Wayland drag support at all:
//! its Wayland backend never creates a `wl_data_device`, so a file dragged
//! onto the window is never offered to us and `DroppedFile` never fires. The
//! implementation exists in winit master and arrives with egui's winit 0.31
//! migration; every other terminal on this desktop accepts a dropped file
//! today, so waiting was the wrong answer.
//!
//! Binding a second data device of our own looks like the obvious fix and is
//! a trap. Mutter keeps **one per client**: `get_data_device` evicts the
//! client's previous device from the list it resolves drags against, so ours
//! would have received the drags *and* the clipboard's selection events, and
//! paste would have quietly stopped working. Verified in
//! `meta-wayland-data-device.c`, and on the wire with `WAYLAND_DEBUG=1`.
//!
//! So drags come from the one device this process has: the clipboard's.
//! `vendor/smithay-clipboard` — the crate eframe uses for the clipboard,
//! patched — forwards them here. This module turns them into paths.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use smithay_clipboard::DragEvent as RawEvent;

/// What the app sees. Positions are surface-local logical pixels, and `None`
/// when the drag is over the window decorations rather than our content —
/// a real position, just not in our coordinate space.
#[derive(Debug, Clone)]
pub enum DragEvent {
    Enter(Option<(f32, f32)>),
    Motion(Option<(f32, f32)>),
    Leave,
    Drop(Vec<PathBuf>),
}

pub struct DragDrop {
    rx: Receiver<DragEvent>,
}

impl DragDrop {
    /// Start receiving drags. `surface` is the raw `wl_surface` of our
    /// content, from eframe's window handle; `wake` is called when an event
    /// lands so the UI repaints without waiting for the next frame.
    pub fn start(surface: *mut std::ffi::c_void, wake: impl Fn() + Send + Sync + 'static) -> Self {
        // Compared by value against the surface each drag arrives on; never
        // dereferenced, so it crosses the thread boundary as an integer.
        let ours = surface as usize;
        let (tx, rx) = channel();
        // Motion reports no surface — it is whichever one was entered — so
        // whether those coordinates mean anything to us has to be remembered
        // from the enter. A drag over the titlebar is over a subsurface of
        // its own, and its coordinates are not in our space.
        let on_content = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered = on_content.clone();
        smithay_clipboard::set_drag_sink(Box::new(move |event| {
            use std::sync::atomic::Ordering::Relaxed;
            let event = match event {
                RawEvent::Enter { x, y, surface } => {
                    entered.store(surface == ours, Relaxed);
                    DragEvent::Enter((surface == ours).then_some((x as f32, y as f32)))
                }
                RawEvent::Motion { x, y } => {
                    DragEvent::Motion(entered.load(Relaxed).then_some((x as f32, y as f32)))
                }
                RawEvent::Leave => DragEvent::Leave,
                RawEvent::Drop(uri_list) => DragEvent::Drop(parse_uri_list(&uri_list)),
            };
            match &event {
                DragEvent::Enter(at) => tracing::debug!("drag entered {at:?}"),
                DragEvent::Drop(paths) => tracing::debug!("dropped {} path(s)", paths.len()),
                _ => {}
            }
            if tx.send(event).is_ok() {
                wake();
            }
        }));
        DragDrop { rx }
    }

    pub fn poll(&self) -> Vec<DragEvent> {
        self.rx.try_iter().collect()
    }
}

/// RFC 2483: one URI per line, CRLF, `#` comments. Anything that is not a
/// local `file://` URI is dropped — a dragged web image has no path to type.
fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("file://"))
        .filter_map(|rest| {
            // `file:///path` has an empty host; `file://localhost/path` is
            // the same file. Any other host is someone else's disk.
            let (host, path) = rest.split_at(rest.find('/')?);
            (host.is_empty() || host == "localhost").then(|| percent_decode(path))
        })
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn percent_decode(s: &str) -> PathBuf {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_file_manager_drop() {
        let list = b"file:///home/yoz/a.png\r\nfile:///home/yoz/b%20c.txt\r\n";
        let paths = parse_uri_list(list);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/yoz/a.png"),
                PathBuf::from("/home/yoz/b c.txt"),
            ]
        );
    }

    #[test]
    fn skips_comments_blanks_and_remote_uris() {
        let list = b"# comment\n\nhttps://example.com/x.png\nfile:///tmp/ok\n";
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/tmp/ok")]);
    }

    #[test]
    fn decodes_utf8_percent_escapes() {
        // Nautilus escapes anything non-ASCII, including Hebrew filenames.
        let list = "file:///tmp/%D7%A9%D7%9C%D7%95%D7%9D.txt".as_bytes();
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/tmp/שלום.txt")]);
    }
}
