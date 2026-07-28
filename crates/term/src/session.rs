//! `TermSession`: one tab's live terminal bundle — Term + io loop + channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::Notify;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode, test::TermSize};
use alacritty_terminal::tty::Pty;
use crossbeam_channel::Receiver;
use parking_lot::RwLock;

use crate::io_loop::{IoLoop, LoopSender, Msg, Notifier, State, WriteBack};
use crate::proxy::{EventProxy, SharedTermState, TabEvent};
use crate::pty::{self, GridSize, SpawnCfg};
use crate::render::theme::Theme;
use crate::tee::Tee;

pub struct TermSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: Receiver<TabEvent>,
    pub shared: Arc<SharedTermState>,
    sender: LoopSender,
    notifier: Notifier,
    dirty: Arc<AtomicBool>,
    size: GridSize,
    handle: Option<JoinHandle<(IoLoop<Pty, EventProxy>, State)>>,
}

impl TermSession {
    pub fn spawn(cfg: &SpawnCfg, egui_ctx: egui::Context, theme: Theme) -> anyhow::Result<Self> {
        let pty = pty::spawn(cfg, 0)?;

        let (tx, events) = crossbeam_channel::unbounded();
        let dirty = Arc::new(AtomicBool::new(true));
        let write_back = Arc::new(WriteBack::default());
        let shared = Arc::new(SharedTermState {
            theme: RwLock::new(theme),
            size: RwLock::new(cfg.size),
        });
        let proxy = EventProxy::new(
            tx,
            egui_ctx,
            dirty.clone(),
            write_back.clone(),
            shared.clone(),
        );

        let term_config = Config {
            scrolling_history: 10_000,
            kitty_keyboard: true,
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &TermSize::new(cfg.size.cols as usize, cfg.size.rows as usize),
            proxy.clone(),
        )));

        let tee = Tee::new(cfg.nonce.clone(), local_hostname());
        let io = IoLoop::new(term.clone(), proxy, pty, tee, write_back.clone(), true)?;
        let sender = io.channel();
        let notifier = Notifier(sender.clone());
        let handle = io.spawn();

        Ok(TermSession {
            term,
            events,
            shared,
            sender,
            notifier,
            dirty,
            size: cfg.size,
            handle: Some(handle),
        })
    }

    /// Write user input to the PTY.
    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.notifier.notify(bytes.into());
    }

    /// Current terminal mode (brief lock).
    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Resize grid + PTY when the geometry changed.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size || size.cols < 2 || size.rows < 2 {
            return;
        }
        self.size = size;
        *self.shared.size.write() = size;
        self.term
            .lock()
            .resize(TermSize::new(size.cols as usize, size.rows as usize));
        let _ = self.sender.send(Msg::Resize(size.into()));
        self.dirty.store(true, Ordering::Release);
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Snap the viewport back to the live (bottom) position.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.dirty.store(true, Ordering::Release);
    }

    /// Scroll the viewport by whole lines (positive = towards history).
    pub fn scroll_lines(&self, lines: i32) {
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// True when terminal content changed since the last call (consumes flag).
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Ask the io loop to stop and join it.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn local_hostname() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                return Some(h.to_string());
            }
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty())
}
