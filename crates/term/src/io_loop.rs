//! PTY I/O loop — forked from `alacritty_terminal` 0.26.0 `event_loop.rs`
//! (Apache-2.0, © the Alacritty project).
//!
//! Deltas vs upstream:
//! 1. Every PTY byte is observed by [`Tee`] *before* `Term::advance`, so
//!    Giverny sees OSC 7/133/9/777 and its private state channel without
//!    touching terminal state.
//! 2. `Event::PtyWrite` replies (DA1/DA2, CPR, kitty-flags reports, …) are
//!    short-circuited into this loop's write queue via [`WriteBack`] instead
//!    of round-tripping through the UI thread — replies stay FIFO ahead of
//!    subsequent user input, and Claude Code's startup probes can't stall on
//!    a busy UI.
//! 3. A single terminal [`LoopHooks::on_loop_done`] callback fires after the
//!    EOF drain, carrying the child's exit status.
//! 4. No ref-test recording.
//!
//! Upstream behaviors deliberately preserved: the `sync_timeout` poll
//! deadline (DECSET 2026 batches flush on time), `MAX_LOCKED_READ` capping
//! how long the terminal lock is held during floods, and EOF draining on
//! child exit.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use alacritty_terminal::event::{self, Event, EventListener, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi;
use alacritty_terminal::{thread, tty};
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::tee::{Tee, TeeEvent};

/// Max bytes to read from the PTY before forced terminal synchronization.
const READ_BUFFER_SIZE: usize = 0x10_0000;

/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

// Poll keys the platform `EventedPty::register` impls use. They are
// `pub(crate)` upstream on unix, so we mirror the values here; the io-loop
// smoke tests fail loudly if they ever drift.
#[cfg(unix)]
const PTY_READ_WRITE_TOKEN: usize = 0;
#[cfg(unix)]
const PTY_CHILD_EVENT_TOKEN: usize = 1;
#[cfg(windows)]
use alacritty_terminal::tty::{PTY_CHILD_EVENT_TOKEN, PTY_READ_WRITE_TOKEN};

/// Messages that may be sent to the io loop.
#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input(Cow<'static, [u8]>),
    /// Shut the loop down (tab close / app exit).
    Shutdown,
    /// Resize the PTY.
    Resize(WindowSize),
}

/// Giverny-specific callbacks, implemented alongside [`EventListener`] by the
/// event proxy. All methods are called on the io thread.
pub trait LoopHooks: EventListener {
    /// Events the tee extracted from the output stream, in order.
    fn on_tee_events(&self, events: Vec<TeeEvent>);
    /// The loop is finished: child exited (with status when known) or a
    /// `Msg::Shutdown` was processed (`None`).
    fn on_loop_done(&self, exit: Option<ExitStatus>);
}

/// Write-back queue for bytes that must reach the PTY without a UI-thread
/// round trip. The event proxy pushes `Event::PtyWrite` payloads here (from
/// within `Term::advance` on the io thread, or from `Term` methods invoked on
/// the UI thread); the poller is nudged so a cross-thread push wakes the loop.
#[derive(Default)]
pub struct WriteBack {
    queue: Mutex<VecDeque<Cow<'static, [u8]>>>,
    waker: OnceLock<Arc<Poller>>,
}

impl WriteBack {
    pub fn push(&self, bytes: Cow<'static, [u8]>) {
        if bytes.is_empty() {
            return;
        }
        self.queue.lock().unwrap().push_back(bytes);
        if let Some(poller) = self.waker.get() {
            let _ = poller.notify();
        }
    }

    fn drain_into(&self, list: &mut VecDeque<Cow<'static, [u8]>>) {
        let mut queue = self.queue.lock().unwrap();
        list.extend(queue.drain(..));
    }
}

/// The forked PTY event loop.
pub struct IoLoop<T: tty::EventedPty, U: LoopHooks> {
    poll: Arc<Poller>,
    pty: T,
    rx: PeekableReceiver<Msg>,
    tx: Sender<Msg>,
    terminal: Arc<FairMutex<Term<U>>>,
    event_proxy: U,
    tee: Tee,
    write_back: Arc<WriteBack>,
    drain_on_exit: bool,
}

impl<T, U> IoLoop<T, U>
where
    T: tty::EventedPty + event::OnResize + Send + 'static,
    U: LoopHooks + Send + 'static,
{
    pub fn new(
        terminal: Arc<FairMutex<Term<U>>>,
        event_proxy: U,
        pty: T,
        tee: Tee,
        write_back: Arc<WriteBack>,
        drain_on_exit: bool,
    ) -> io::Result<IoLoop<T, U>> {
        let (tx, rx) = mpsc::channel();
        let poll: Arc<Poller> = Poller::new()?.into();
        let _ = write_back.waker.set(poll.clone());
        Ok(IoLoop {
            poll,
            pty,
            tx,
            rx: PeekableReceiver::new(rx),
            terminal,
            event_proxy,
            tee,
            write_back,
            drain_on_exit,
        })
    }

    pub fn channel(&self) -> LoopSender {
        LoopSender {
            sender: self.tx.clone(),
            poller: self.poll.clone(),
        }
    }

    /// Drain the channel; `false` when a shutdown message was received.
    fn drain_recv_channel(&mut self, state: &mut State) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input(input) => state.write_list.push_back(input),
                Msg::Resize(window_size) => self.pty.on_resize(window_size),
                Msg::Shutdown => return false,
            }
        }
        true
    }

    #[inline]
    fn pty_read(&mut self, state: &mut State, buf: &mut [u8]) -> io::Result<()> {
        let mut unprocessed = 0;
        let mut processed = 0;

        // Reserve the next terminal lock for PTY reading.
        let _terminal_lease = Some(self.terminal.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                // Received on Windows/macOS when no more data is readable.
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => {
                    // Giverny delta: tee sees every byte exactly once, before
                    // terminal state advances and independent of locking.
                    self.tee.observe(&buf[unprocessed..unprocessed + got]);
                    unprocessed += got;
                }
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // Go back to polling if we're caught up and the PTY
                        // would block.
                        if unprocessed == 0 {
                            break;
                        }
                    }
                    _ => return Err(err),
                },
            }

            // Attempt to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.terminal.try_lock_unfair() {
                    // Force block if we are at the buffer size limit.
                    None if unprocessed >= READ_BUFFER_SIZE => self.terminal.lock_unfair(),
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Parse the incoming bytes.
            state.parser.advance(&mut **terminal, &buf[..unprocessed]);

            processed += unprocessed;
            unprocessed = 0;

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        // Giverny delta: surface tee events and any probe replies queued
        // during `advance` (the proxy routes `Event::PtyWrite` into
        // `write_back`, keeping replies ahead of later user input).
        if self.tee.has_events() {
            self.event_proxy.on_tee_events(self.tee.take_events());
        }
        self.write_back.drain_into(&mut state.write_list);

        // Queue terminal redraw unless all processed bytes were synchronized.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.event_proxy.send_event(Event::Wakeup);
        }

        Ok(())
    }

    #[inline]
    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();

        'write_many: while let Some(mut current) = state.take_current() {
            'write_one: loop {
                match self.pty.writer().write(current.remaining_bytes()) {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    }
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    }
                    Err(err) => {
                        state.set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn spawn(mut self) -> JoinHandle<(Self, State)> {
        thread::spawn_named("giverny pty io", move || {
            let mut state = State::default();
            let mut buf = vec![0u8; READ_BUFFER_SIZE];
            let mut exit_status: Option<ExitStatus> = None;

            let poll_opts = PollMode::Level;
            let mut interest = PollingEvent::readable(0);

            // Register TTY through EventedRW interface.
            if let Err(err) = unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                tracing::error!("io loop registration error: {err}");
                self.event_proxy.on_loop_done(None);
                return (self, state);
            }

            let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());

            'event_loop: loop {
                // Wake up when a synchronized-update timeout is reached.
                let handler = state.parser.sync_timeout();
                let timeout = handler
                    .sync_timeout()
                    .map(|st| st.saturating_duration_since(Instant::now()));

                events.clear();
                if let Err(err) = self.poll.wait(&mut events, timeout) {
                    match err.kind() {
                        ErrorKind::Interrupted => continue,
                        _ => {
                            tracing::error!("io loop polling error: {err}");
                            break 'event_loop;
                        }
                    }
                }

                // Handle synchronized update timeout.
                if events.is_empty() && self.rx.peek().is_none() {
                    state.parser.stop_sync(&mut *self.terminal.lock());
                    self.event_proxy.send_event(Event::Wakeup);
                    continue;
                }

                // Handle channel events, if there are any.
                if !self.drain_recv_channel(&mut state) {
                    break;
                }
                // Cross-thread probe replies (e.g. Term methods driven by the
                // UI thread) land in write_back; merge them each iteration.
                self.write_back.drain_into(&mut state.write_list);

                for event in events.iter() {
                    match event.key {
                        PTY_CHILD_EVENT_TOKEN => {
                            if let Some(tty::ChildEvent::Exited(status)) =
                                self.pty.next_child_event()
                            {
                                exit_status = status;
                                if let Some(status) = status {
                                    self.event_proxy.send_event(Event::ChildExit(status));
                                }
                                if self.drain_on_exit {
                                    let _ = self.pty_read(&mut state, &mut buf);
                                }
                                self.terminal.lock().exit();
                                self.event_proxy.send_event(Event::Wakeup);
                                break 'event_loop;
                            }
                        }

                        PTY_READ_WRITE_TOKEN => {
                            if event.is_interrupt() {
                                // Don't try to do I/O on a dead PTY.
                                continue;
                            }

                            if event.readable
                                && let Err(err) = self.pty_read(&mut state, &mut buf)
                            {
                                // On Linux, a `read` on the master side of
                                // a PTY can fail with `EIO` if the client
                                // side hangs up; loop back around for the
                                // inevitable `Exited` event.
                                #[cfg(target_os = "linux")]
                                if err.raw_os_error() == Some(libc::EIO) {
                                    continue;
                                }

                                tracing::error!("error reading from pty: {err}");
                                break 'event_loop;
                            }

                            if event.writable
                                && let Err(err) = self.pty_write(&mut state)
                            {
                                tracing::error!("error writing to pty: {err}");
                                break 'event_loop;
                            }
                        }
                        _ => (),
                    }
                }

                // Register write interest if necessary.
                let needs_write = state.needs_write();
                if needs_write != interest.writable {
                    interest.writable = needs_write;
                    self.pty
                        .reregister(&self.poll, interest, poll_opts)
                        .unwrap();
                }
            }

            // Flush any tee events observed during the final drain.
            if self.tee.has_events() {
                self.event_proxy.on_tee_events(self.tee.take_events());
            }
            self.event_proxy.on_loop_done(exit_status);

            // The evented instances are not dropped here, so deregister explicitly.
            let _ = self.pty.deregister(&self.poll);

            (self, state)
        })
    }
}

/// Handle for sending messages to a running io loop.
#[derive(Clone)]
pub struct LoopSender {
    sender: Sender<Msg>,
    poller: Arc<Poller>,
}

impl LoopSender {
    pub fn send(&self, msg: Msg) -> Result<(), SendError> {
        self.sender.send(msg).map_err(SendError::Send)?;
        self.poller.notify().map_err(SendError::Io)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("io loop poller error: {0}")]
    Io(io::Error),
    #[error("io loop channel closed: {0}")]
    Send(mpsc::SendError<Msg>),
}

/// Adapter implementing `alacritty_terminal`'s input/resize traits over a
/// [`LoopSender`] — the UI thread's handle for writing to the PTY.
pub struct Notifier(pub LoopSender);

impl event::Notify for Notifier {
    fn notify<B>(&self, bytes: B)
    where
        B: Into<Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        // Terminal hangs if we send 0 bytes through.
        if bytes.is_empty() {
            return;
        }
        let _ = self.0.send(Msg::Input(bytes));
    }
}

impl event::OnResize for Notifier {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.0.send(Msg::Resize(window_size));
    }
}

/// Mutable loop state: pending writes and the VT processor.
#[derive(Default)]
pub struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
}

impl State {
    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

/// Tracks how much of a buffer has been written.
struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing {
            source: c,
            written: 0,
        }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

struct PeekableReceiver<T> {
    rx: Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }
        self.peeked.as_ref()
    }

    fn recv(&mut self) -> Option<T> {
        if self.peeked.is_some() {
            self.peeked.take()
        } else {
            match self.rx.try_recv() {
                Err(TryRecvError::Disconnected) => panic!("io loop channel closed"),
                res => res.ok(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{self, GridSize, SpawnCfg};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, test::TermSize};
    use crossbeam_channel::{Receiver as XReceiver, Sender as XSender, unbounded};
    use std::time::Duration;

    #[derive(Debug)]
    enum TestMsg {
        Event(#[allow(dead_code, reason = "kept for debug printing")] String),
        Tee(Vec<TeeEvent>),
        Done(Option<ExitStatus>),
    }

    #[derive(Clone)]
    struct TestProxy {
        tx: XSender<TestMsg>,
        write_back: Arc<WriteBack>,
    }

    impl EventListener for TestProxy {
        fn send_event(&self, event: Event) {
            if let Event::PtyWrite(text) = event {
                // Production proxies do exactly this: short-circuit replies.
                self.write_back.push(text.into_bytes().into());
                return;
            }
            let _ = self.tx.send(TestMsg::Event(format!("{event:?}")));
        }
    }

    impl LoopHooks for TestProxy {
        fn on_tee_events(&self, events: Vec<TeeEvent>) {
            let _ = self.tx.send(TestMsg::Tee(events));
        }
        fn on_loop_done(&self, exit: Option<ExitStatus>) {
            let _ = self.tx.send(TestMsg::Done(exit));
        }
    }

    struct Harness {
        term: Arc<FairMutex<Term<TestProxy>>>,
        rx: XReceiver<TestMsg>,
        _sender: LoopSender,
        handle: JoinHandle<(IoLoop<tty::Pty, TestProxy>, State)>,
    }

    fn run(script: &str, hostname: &str) -> Harness {
        let cfg = SpawnCfg {
            shell: Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
            cwd: std::env::temp_dir(),
            env_extra: vec![],
            tab_id: "t".into(),
            nonce: "n".into(),
            claude_config_dir: None,
            size: GridSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
        };
        let pty = pty::spawn(&cfg, 0).expect("spawn pty");
        let (tx, rx) = unbounded();
        let write_back = Arc::new(WriteBack::default());
        let proxy = TestProxy {
            tx,
            write_back: clone_arc(&write_back),
        };
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TermSize::new(80, 24),
            proxy.clone(),
        )));
        let looper = IoLoop::new(
            term.clone(),
            proxy,
            pty,
            Tee::new("n".into(), Some(hostname.into())),
            write_back,
            true,
        )
        .expect("io loop");
        let sender = looper.channel();
        let handle = looper.spawn();
        Harness {
            term,
            rx,
            _sender: sender,
            handle,
        }
    }

    fn clone_arc(a: &Arc<WriteBack>) -> Arc<WriteBack> {
        Arc::clone(a)
    }

    fn wait_done(h: &Harness) -> (Vec<Vec<TeeEvent>>, Option<ExitStatus>) {
        let mut tees = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match h.rx.recv_timeout(remaining) {
                Ok(TestMsg::Done(exit)) => return (tees, exit),
                Ok(TestMsg::Tee(ev)) => tees.push(ev),
                Ok(TestMsg::Event(_)) => {}
                Err(_) => panic!("io loop did not finish in time"),
            }
        }
    }

    fn screen_text(term: &Arc<FairMutex<Term<TestProxy>>>) -> String {
        let term = term.lock();
        let grid = term.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() {
            for col in 0..grid.columns() {
                let point = alacritty_terminal::index::Point::new(
                    alacritty_terminal::index::Line(line as i32),
                    alacritty_terminal::index::Column(col),
                );
                out.push(grid[point].c);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn child_output_reaches_term_and_loop_finishes() {
        let h = run("printf 'giverny-hello'", "testhost");
        let (_tees, exit) = wait_done(&h);
        assert!(
            exit.is_some_and(|s| s.success()),
            "child should exit cleanly"
        );
        let text = screen_text(&h.term);
        assert!(
            text.contains("giverny-hello"),
            "grid should contain output, got:\n{text}"
        );
        h.handle.join().unwrap();
    }

    #[test]
    fn tee_sees_osc7_from_child() {
        let h = run(
            r#"printf '\033]7;file://testhost/tmp/tee-dir\007after'"#,
            "testhost",
        );
        let (tees, exit) = wait_done(&h);
        assert!(exit.is_some_and(|s| s.success()));
        let all: Vec<TeeEvent> = tees.into_iter().flatten().collect();
        assert!(
            all.contains(&TeeEvent::CwdChanged("/tmp/tee-dir".into())),
            "expected CwdChanged, got {all:?}"
        );
        // The OSC bytes must ALSO have reached the real terminal untouched
        // (tee is read-only): the trailing text renders.
        assert!(screen_text(&h.term).contains("after"));
        h.handle.join().unwrap();
    }

    #[test]
    fn da1_probe_reply_short_circuits_to_child() {
        // Child sends DA1 (ESC [ c); Term emits Event::PtyWrite with the
        // reply; the proxy short-circuits it into write_back; the loop writes
        // it to the PTY; the child reads until the reply's spec-guaranteed
        // final byte `c`, then prints a marker.
        let h = run(
            concat!(
                "stty raw -echo 2>/dev/null; printf '\\033[c'; ",
                "while :; do b=$(dd bs=1 count=1 2>/dev/null); ",
                "case \"$b\" in c) break;; esac; done; ",
                "printf 'reply-ok'"
            ),
            "testhost",
        );
        let (_tees, exit) = wait_done(&h);
        assert!(exit.is_some_and(|s| s.success()));
        let text = screen_text(&h.term);
        assert!(
            text.contains("reply-ok"),
            "child never saw the DA1 reply; grid:\n{text}"
        );
        h.handle.join().unwrap();
    }

    #[test]
    fn shutdown_message_ends_loop() {
        let h = run("sleep 30", "testhost");
        h._sender.send(Msg::Shutdown).unwrap();
        let (_tees, exit) = wait_done(&h);
        assert!(exit.is_none(), "shutdown path reports no exit status");
        h.handle.join().unwrap();
    }
}
