//! Production event proxy: routes `alacritty_terminal` events and tee events
//! from the io thread to the app, answering protocol queries inline.

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::event::{Event, EventListener};
use crossbeam_channel::Sender;
use parking_lot::RwLock;

use crate::io_loop::{LoopHooks, WriteBack};
use crate::pty::GridSize;
use crate::render::theme::Theme;
use crate::tee::TeeEvent;

/// OSC 52 store payload cap (decoded), per the design review.
const CLIPBOARD_STORE_CAP: usize = 8 * 1024 * 1024;

/// App-facing events from one tab's terminal (per-tab channel).
#[derive(Debug)]
pub enum TabEvent {
    /// OSC 0/2 title; `None` = reset to default.
    Title(Option<String>),
    Bell,
    Tee(Vec<TeeEvent>),
    ChildExit(ExitStatus),
    /// The io loop is finished (child exit or shutdown).
    LoopDone(Option<ExitStatus>),
}

/// State shared between the UI thread and the proxy for inline replies.
pub struct SharedTermState {
    pub theme: RwLock<Theme>,
    pub size: RwLock<GridSize>,
}

#[derive(Clone)]
pub struct EventProxy {
    tx: Sender<TabEvent>,
    ctx: egui::Context,
    dirty: Arc<AtomicBool>,
    write_back: Arc<WriteBack>,
    shared: Arc<SharedTermState>,
}

impl EventProxy {
    pub fn new(
        tx: Sender<TabEvent>,
        ctx: egui::Context,
        dirty: Arc<AtomicBool>,
        write_back: Arc<WriteBack>,
        shared: Arc<SharedTermState>,
    ) -> Self {
        Self {
            tx,
            ctx,
            dirty,
            write_back,
            shared,
        }
    }

    fn wake(&self) {
        // Coalesce: only the false→true edge requests a repaint.
        if !self.dirty.swap(true, Ordering::AcqRel) {
            self.ctx.request_repaint();
        }
    }

    fn send(&self, ev: TabEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.wake(),
            Event::PtyWrite(text) => self.write_back.push(text.into_bytes().into()),
            Event::Title(title) => self.send(TabEvent::Title(Some(title))),
            Event::ResetTitle => self.send(TabEvent::Title(None)),
            Event::Bell => self.send(TabEvent::Bell),
            Event::ChildExit(status) => self.send(TabEvent::ChildExit(status)),
            Event::ClipboardStore(_, text) => {
                if text.len() <= CLIPBOARD_STORE_CAP {
                    self.ctx.copy_text(text);
                }
            }
            Event::ClipboardLoad(_, formatter) => {
                // Default-deny: programs may not read the clipboard.
                self.write_back.push(formatter("").into_bytes().into());
            }
            Event::ColorRequest(index, formatter) => {
                let theme = self.shared.theme.read();
                let c = match index {
                    0..=255 => theme.indexed(index as u8),
                    256 => theme.fg,
                    257 => theme.bg,
                    258 => theme.cursor,
                    _ => theme.fg,
                };
                let rgb = alacritty_terminal::vte::ansi::Rgb {
                    r: c.r(),
                    g: c.g(),
                    b: c.b(),
                };
                self.write_back.push(formatter(rgb).into_bytes().into());
            }
            Event::TextAreaSizeRequest(formatter) => {
                let size = *self.shared.size.read();
                let ws: alacritty_terminal::event::WindowSize = size.into();
                self.write_back.push(formatter(ws).into_bytes().into());
            }
            Event::MouseCursorDirty | Event::CursorBlinkingChange => self.wake(),
            Event::Exit => {}
        }
    }
}

impl LoopHooks for EventProxy {
    fn on_tee_events(&self, events: Vec<TeeEvent>) {
        self.send(TabEvent::Tee(events));
    }

    fn on_loop_done(&self, exit: Option<ExitStatus>) {
        self.send(TabEvent::LoopDone(exit));
    }
}
