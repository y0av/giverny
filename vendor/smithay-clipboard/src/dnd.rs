//! Drag-and-drop, forwarded to the application.
//!
//! Not upstream. The clipboard thread already owns this client's only
//! `wl_data_device` (see `README.md`), and a data device carries drags as
//! well as selections — upstream simply ignores the drag half. This module is
//! the way out: the application installs a sink, and the drag events the
//! handlers in `state.rs` receive are handed to it.
//!
//! The sink is process-global because the `Clipboard` is created deep inside
//! the windowing library, where the application never sees the object.

use std::sync::Mutex;

/// The one mime type a file drag is worth reporting: a list of `file://`
/// URIs, which is what file managers, browsers and GTK all send.
pub const URI_LIST: &str = "text/uri-list";

#[derive(Debug, Clone)]
pub enum DragEvent {
    /// A drag carrying files entered a surface. Coordinates are surface-local
    /// logical pixels; `surface` is the raw `wl_surface` pointer, so an
    /// application with more than one surface can tell which.
    Enter { x: f64, y: f64, surface: usize },
    Motion { x: f64, y: f64 },
    /// The drag left, or was cancelled.
    Leave,
    /// Dropped: the raw `text/uri-list` payload, unparsed.
    Drop(Vec<u8>),
}

type Sink = Box<dyn Fn(DragEvent) + Send + Sync + 'static>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Receive drag-and-drop events. Replaces any previous sink.
///
/// Install it before the first drag; events that arrive with no sink are
/// dropped, and the drag is still refused politely rather than left hanging.
pub fn set_drag_sink(sink: Sink) {
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(sink);
    }
}

/// Whether anyone is listening — the handlers refuse drags when nobody is, so
/// the source shows a "no" cursor instead of a drop that goes nowhere.
pub(crate) fn wanted() -> bool {
    SINK.lock().is_ok_and(|slot| slot.is_some())
}

pub(crate) fn emit(event: DragEvent) {
    if let Ok(slot) = SINK.lock()
        && let Some(sink) = slot.as_ref()
    {
        sink(event);
    }
}
