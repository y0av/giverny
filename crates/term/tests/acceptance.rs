//! M1 acceptance tests: byte-exact probe replies, flood responsiveness, and
//! the real Claude Code binary running inside a headless Giverny session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::{Config, Term, test::TermSize};
use alacritty_terminal::vte::ansi::Processor;

use giverny_term::proxy::TabEvent;
use giverny_term::pty::{GridSize, SpawnCfg};
use giverny_term::render::theme::Theme;
use giverny_term::session::TermSession;

/// Captures `PtyWrite` replies emitted by `Term` during `advance`.
#[derive(Clone, Default)]
struct ReplyCapture(Arc<Mutex<Vec<String>>>);

impl EventListener for ReplyCapture {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.0.lock().unwrap().push(text);
        }
    }
}

/// Golden test: the exact reply bytes Claude Code's startup probes get.
#[test]
fn probe_replies_are_byte_exact() {
    let capture = ReplyCapture::default();
    let config = Config {
        kitty_keyboard: true,
        ..Config::default()
    };
    let mut term = Term::new(config, &TermSize::new(80, 24), capture.clone());
    let mut parser: Processor = Processor::new();

    let mut probe = |bytes: &[u8]| -> Vec<String> {
        parser.advance(&mut term, bytes);
        std::mem::take(&mut *capture.0.lock().unwrap())
    };

    // DA1 (ESC [ c) — primary device attributes.
    let da1 = probe(b"\x1b[c");
    assert_eq!(da1, vec!["\x1b[?6c".to_string()], "DA1 reply");

    // DA2 (ESC [ > c) — secondary device attributes: ends in 'c'.
    let da2 = probe(b"\x1b[>0c");
    assert_eq!(da2.len(), 1, "DA2 must reply, got {da2:?}");
    assert!(
        da2[0].starts_with("\x1b[>") && da2[0].ends_with('c'),
        "DA2 shape: {:?}",
        da2[0]
    );

    // CPR (ESC [ 6 n) — cursor position report, 1-based.
    let cpr = probe(b"\x1b[6n");
    assert_eq!(cpr, vec!["\x1b[1;1R".to_string()], "CPR reply");

    // Kitty keyboard flags query (ESC [ ? u).
    let kitty = probe(b"\x1b[?u");
    assert_eq!(
        kitty.len(),
        1,
        "kitty flags query must reply (kitty_keyboard on)"
    );
    assert!(
        kitty[0].starts_with("\x1b[?") && kitty[0].ends_with('u'),
        "kitty reply shape: {:?}",
        kitty[0]
    );

    // Synchronized output (DECSET 2026): vte buffers the whole stream during
    // a batch, so a probe inside one gets no reply *until the batch closes*
    // (or the io loop's sync-timeout deadline fires — ported in io_loop.rs).
    // Encode that contract: silent mid-batch, flushed reply on close.
    probe(b"\x1b[?2026h");
    let mid = probe(b"\x1b[c");
    assert!(
        mid.is_empty(),
        "replies buffer during a sync batch, got {mid:?}"
    );
    let flushed = probe(b"\x1b[?2026l");
    assert_eq!(
        flushed,
        vec!["\x1b[?6c".to_string()],
        "buffered probe reply must flush when the batch closes"
    );
}

fn headless_session(
    script_shell: Option<(String, Vec<String>)>,
    cwd: PathBuf,
    config_dir: Option<PathBuf>,
) -> TermSession {
    headless_session_preseeded(script_shell, cwd, config_dir, None)
}

fn headless_session_preseeded(
    script_shell: Option<(String, Vec<String>)>,
    cwd: PathBuf,
    config_dir: Option<PathBuf>,
    preseed: Option<&str>,
) -> TermSession {
    let cfg = SpawnCfg {
        shell: script_shell,
        cwd,
        env_extra: vec![],
        tab_id: "test-tab".into(),
        nonce: "test-nonce".into(),
        claude_config_dir: config_dir,
        size: GridSize {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
        },
    };
    TermSession::spawn(&cfg, egui::Context::default(), Theme::monet_dark(), preseed)
        .expect("session spawns")
}

fn wait_for<F: FnMut() -> bool>(mut f: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Flood test: 20 MB through the PTY; the terminal lock must stay responsive
/// (UI thread never starves behind the reader).
#[test]
fn flood_keeps_lock_responsive() {
    let session = headless_session(
        Some((
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "yes 0123456789abcdef | head -c 20000000".into(),
            ],
        )),
        std::env::temp_dir(),
        None,
    );

    let start = Instant::now();
    let mut worst_lock = Duration::ZERO;
    let mut done = false;
    while start.elapsed() < Duration::from_secs(30) && !done {
        let t0 = Instant::now();
        drop(session.term.lock());
        worst_lock = worst_lock.max(t0.elapsed());
        while let Ok(ev) = session.events.try_recv() {
            if matches!(ev, TabEvent::LoopDone(_)) {
                done = true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(done, "flood child should finish within 30s");
    assert!(
        worst_lock < Duration::from_millis(150),
        "terminal lock starved during flood: worst acquisition {worst_lock:?}"
    );
    session.shutdown();
}

fn drain_until_done(session: &TermSession) {
    let ok = wait_for(
        || {
            let mut done = false;
            while let Ok(ev) = session.events.try_recv() {
                if matches!(ev, TabEvent::LoopDone(_)) {
                    done = true;
                }
            }
            done
        },
        Duration::from_secs(10),
    );
    assert!(ok, "session did not finish");
}

/// The M3 restore contract: scrollback from one session, serialized to an
/// ANSI dump, pre-seeds a fresh session — text, colors, and ordering intact,
/// with the restored divider between old content and the new shell.
#[test]
fn scrollback_survives_restart_via_preseed() {
    let a = headless_session(
        Some((
            "/bin/sh".into(),
            vec![
                "-c".into(),
                r"printf '\033[31mpoppy\033[0m plain \033[1mbold\033[0m\r\n'".into(),
            ],
        )),
        std::env::temp_dir(),
        None,
    );
    drain_until_done(&a);
    let dump = a.snapshot_ansi(1000).expect("snapshot from session A");
    assert!(dump.contains("poppy"), "dump has text: {dump:?}");
    assert!(
        dump.contains("\x1b[0;1"),
        "dump re-emits bold SGR: {dump:?}"
    );
    a.shutdown();

    let b = headless_session_preseeded(
        Some((
            "/bin/sh".into(),
            vec!["-c".into(), "printf 'fresh-prompt'".into()],
        )),
        std::env::temp_dir(),
        None,
        Some(&dump),
    );
    drain_until_done(&b);
    let screen = b.screen_text();
    let old = screen
        .find("poppy plain bold")
        .expect("restored content on screen");
    let divider = screen.find("── restored ──").expect("divider on screen");
    let fresh = screen
        .find("fresh-prompt")
        .expect("fresh shell output on screen");
    assert!(
        old < divider && divider < fresh,
        "order: restored < divider < fresh\n{screen}"
    );
    b.shutdown();
}

/// Tab identity must reach hook subprocesses: the env chain is
/// giverny → shell → claude → hook, so the shell must see it.
#[test]
fn tab_env_reaches_child() {
    let h = headless_session(
        Some((
            "/bin/sh".into(),
            vec!["-c".into(), "printf 'TAB=[%s]' \"$GIVERNY_TAB_ID\"".into()],
        )),
        std::env::temp_dir(),
        None,
    );
    drain_until_done(&h);
    let screen = h.screen_text();
    assert!(
        screen.contains("TAB=[test-tab]"),
        "GIVERNY_TAB_ID missing:\n{screen}"
    );
    h.shutdown();
}

fn find_claude() -> Option<String> {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v claude"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// The real Claude Code binary runs inside a headless Giverny session with a
/// scratch CLAUDE_CONFIG_DIR: its startup terminal probes must succeed and
/// its onboarding/login TUI must render. No login, no API usage.
#[test]
fn claude_tui_renders_in_session() {
    let Some(claude) = find_claude() else {
        eprintln!("SKIP: claude binary not found");
        return;
    };

    let scratch =
        std::env::temp_dir().join(format!("giverny-claude-accept-{}", std::process::id()));
    let config_dir = scratch.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let session = headless_session(Some((claude, vec![])), scratch.clone(), Some(config_dir));

    let rendered = wait_for(
        || {
            let text = session.screen_text();
            let t = text.to_lowercase();
            t.contains("claude") || t.contains("welcome") || t.contains("theme")
        },
        Duration::from_secs(25),
    );
    let final_text = session.screen_text();
    let mode = session.mode();
    eprintln!("--- claude screen ---\n{final_text}\n--- mode: {mode:?} ---");
    assert!(rendered, "claude TUI never rendered; screen:\n{final_text}");

    // A real TUI got composed: several non-empty rows.
    let rows = final_text.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(
        rows >= 3,
        "expected a composed TUI, got {rows} non-empty rows:\n{final_text}"
    );

    session.shutdown();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// An OSC 8 hyperlink puts the URL in the escape sequence and only a label on
/// screen. Claude prints links this way, and so do `gh`, `delta` and
/// `ls --hyperlink` — so the visible text is never enough to find the target.
#[test]
fn osc8_hyperlinks_are_recoverable_from_the_grid() {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line, Point};

    let mut term = Term::new(
        Config::default(),
        &TermSize::new(80, 24),
        ReplyCapture::default(),
    );
    let mut parser: Processor = Processor::new();
    parser.advance(
        &mut term,
        b"see \x1b]8;;https://example.com/docs\x1b\\click here\x1b]8;;\x1b\\ ok",
    );

    let grid = term.grid();
    let row: String = (0..grid.columns())
        .map(|c| grid[Point::new(Line(0), Column(c))].c)
        .collect();
    assert!(row.starts_with("see click here ok"), "{row:?}");
    // The URL is nowhere in the text — which is exactly why the cell metadata
    // has to be consulted.
    assert!(!row.contains("example.com"));

    let link_at = |col: usize| {
        grid[Point::new(Line(0), Column(col))]
            .hyperlink()
            .map(|h| h.uri().to_string())
    };
    // "click here" spans columns 4..=13.
    assert_eq!(link_at(4).as_deref(), Some("https://example.com/docs"));
    assert_eq!(link_at(13).as_deref(), Some("https://example.com/docs"));
    // The label's neighbours are not part of the link.
    assert_eq!(link_at(3), None, "space before the label");
    assert_eq!(link_at(15), None, "text after the label");
}
