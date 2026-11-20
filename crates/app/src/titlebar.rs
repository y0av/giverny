//! Giverny's own title bar, for WSLg (#69).
//!
//! Under WSLg the window is an X11 one (see `wslg::avoid_wayland`), and an
//! X11 window's frame there is drawn by WSLg's Weston: a 1×-scale caption a
//! few pixels tall with Windows-95 buttons, inside a thick black band where
//! Weston's shadow would be. Windows never draws a caption for it, and there
//! is no setting that asks it to. So on WSLg the window opens without
//! decorations and draws this instead — a Windows 11 caption in the theme's
//! colours, at the interface's own scale — and the edges it no longer has are
//! handed back as resize handles.
//!
//! Moving and resizing go to the window manager as `_NET_WM_MOVERESIZE`
//! (winit's `drag_window` / `drag_resize_window`), which WSLg passes on to
//! Windows, so a drag feels like dragging any other window.
//!
//! Maximising is the one thing the window manager cannot be left to do
//! (#78): see [`Maximize`].

use crate::chrome::Chrome;
use eframe::egui::{self, Color32, CursorIcon, ResizeDirection, Sense, Stroke, ViewportCommand};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Caption height, in points: Windows 11's 32 px at 100 %.
pub const HEIGHT: f32 = 32.0;

/// Width of a caption button: Windows 11's 46 px.
const BUTTON_W: f32 = 46.0;

/// How far in from the window's edge a press resizes rather than clicks, in
/// points. Windows' own invisible resize border is about this wide.
const EDGE: f32 = 5.0;

/// Windows 11's close-button red, and the white its glyph turns on it.
const CLOSE_RED: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);

/// Maximising a frameless window by hand, because WSLg gets it wrong (#78).
///
/// Weston's X window manager keeps a 32-px shadow margin round every
/// undecorated window, maximised or not, and WSLg sends the whole surface,
/// margin included, to Windows. Windows puts a maximised window's top-left
/// at the work area's corner — the margin's corner, not the window's — so a
/// maximised Giverny sat 32 px down and to the right, its right and bottom
/// 32 px past the screen. Weston's own frame drops the margin when
/// maximised, which is why a decorated window never showed it. The margin
/// is keyed on nothing a client can set (not Motif hints, not the window
/// type), so it cannot be asked away.
///
/// So the window is never left maximised: it is laid over the work area by
/// hand, one move and one resize, which an ordinary window takes exactly
/// (its margin falls off-screen). The work area comes from Windows, asked
/// in the background at startup ([`probe_work_areas`]) and again once X's
/// monitors change, and is kept in the saved layout so the next launch has
/// it before the window exists. Until either has it, and whenever Windows
/// maximises the window itself (Win+↑, a drag to the top), the window
/// manager's maximise is what gives the work area away, and it is undone
/// and redone by hand.
///
/// Windows' pixels are not X's (#540): see [`work_area_on`].
#[derive(Default)]
pub struct Maximize {
    /// Laid over the work area by hand.
    on: bool,
    /// The work area, in physical pixels, until the window is laid over it,
    /// and when that was asked for.
    pending: Option<(egui::Rect, Instant)>,
    /// Where it was laid, in physical pixels, once it got there.
    placed: Option<egui::Rect>,
    /// The inner rect, in physical pixels, to go back to on restore.
    restore: Option<egui::Rect>,
    /// The window manager's maximise being undone: the work area it gave
    /// away, in physical pixels, and when.
    unmaximizing: Option<(egui::Rect, Instant)>,
    /// A move held back until a shrink has landed (see [`Maximize::place`]):
    /// the corner and size in points, and when the shrink was asked for.
    deferred_move: Option<(egui::Pos2, egui::Vec2, Instant)>,
    /// The window's own size, in physical pixels, when it opened maximised
    /// and has no rect of its own to restore to yet.
    restore_size: Option<egui::Vec2>,
    /// The window's rect before the window manager maximised it (or as it
    /// opened), in physical pixels: until the maximised rect differs from
    /// it, the window manager has not got there yet.
    before: Option<egui::Rect>,
    /// When the window manager's maximise was first seen.
    max_since: Option<Instant>,
    /// The last work area learned, from Windows or the window manager, in
    /// physical pixels; saved with the layout.
    learned: Option<egui::Rect>,
}

/// How long the window manager gets to carry out a placement before it is
/// asked again, and before it is given up on.
const RESEND: Duration = Duration::from_millis(300);
const GIVE_UP: Duration = Duration::from_millis(1500);

impl Maximize {
    /// Start with the work area saved last time, if any, and — when the
    /// window opened already laid over it — maximised, restoring to
    /// `restore_size` (physical pixels) centred on it.
    ///
    /// Opened maximised without a saved work area (a state file from before
    /// #78), the window manager maximises it, and `restore_size` waits for
    /// the work area that gives.
    pub fn new(
        learned: Option<[f32; 4]>,
        opened_maximized: bool,
        restore_size: egui::Vec2,
    ) -> Self {
        let learned = learned
            .map(|[x, y, w, h]| egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h)));
        let mut m = Maximize {
            learned,
            restore_size: opened_maximized.then_some(restore_size),
            ..Default::default()
        };
        if let (true, Some(area)) = (opened_maximized, learned) {
            m.on = true;
            m.pending = Some((area, Instant::now()));
            m.restore = Some(egui::Rect::from_center_size(
                area.center(),
                restore_size.min(area.size()),
            ));
        }
        m
    }

    /// The work area to save, `[x, y, w, h]` in physical pixels.
    pub fn learned(&self) -> Option<[f32; 4]> {
        self.learned
            .map(|r| [r.min.x, r.min.y, r.width(), r.height()])
    }

    /// Whether the window is maximised, by the window manager or by hand.
    pub fn is_on(&self, ctx: &egui::Context) -> bool {
        self.on || ctx.input(|i| i.viewport().maximized.unwrap_or(false))
    }

    /// Once a frame, before anything reads [`Maximize::is_on`].
    pub fn update(&mut self, ctx: &egui::Context) {
        let ppp = ctx.pixels_per_point();
        let (maximized, fullscreen, inner) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.maximized.unwrap_or(false),
                vp.fullscreen.unwrap_or(false),
                vp.inner_rect,
            )
        });
        let Some(inner) = inner else {
            return;
        };
        if fullscreen {
            return;
        }
        if let Some((corner, size, since)) = self.deferred_move {
            if (inner.size() - size).length() < 2.0 || since.elapsed() > GIVE_UP {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(corner));
                self.deferred_move = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(8));
            }
        }
        let now = px(inner, ppp);
        let before = *self.before.get_or_insert(now);
        if maximized {
            // The window manager maximised it (no work area known yet, or
            // Windows did it). Undo that, and once the undo has landed lay
            // the window over the work area by hand; placed before, the
            // un-maximise would move it back.
            if self.unmaximizing.is_none() {
                let Some(area) = self
                    .work_area(ctx)
                    .or_else(|| self.maximized_rect(ctx, now, before))
                else {
                    ctx.request_repaint_after(Duration::from_millis(16));
                    return;
                };
                self.learned = Some(area);
                if !self.on {
                    let size = self.restore_size.take().map(|s| s.min(area.size()));
                    self.restore.get_or_insert_with(|| {
                        egui::Rect::from_center_size(
                            area.center(),
                            size.unwrap_or(area.size() * 0.75),
                        )
                    });
                }
                ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                self.unmaximizing = Some((area, Instant::now()));
            }
            self.on = true;
            ctx.request_repaint_after(Duration::from_millis(8));
            return;
        }
        self.max_since = None;
        if let Some((area, since)) = self.unmaximizing {
            // The flag clears before the window is back at its own size.
            if near(now, area) && since.elapsed() < RESEND {
                ctx.request_repaint_after(Duration::from_millis(8));
                return;
            }
            self.unmaximizing = None;
            self.lay_over(ctx, area);
            return;
        }
        if let Some((area, since)) = self.pending {
            let waited = since.elapsed();
            if near(now, area) || waited > GIVE_UP {
                self.pending = None;
                self.placed = Some(now);
            } else {
                if waited > RESEND && waited < RESEND + Duration::from_millis(50) {
                    self.place(ctx, area.min / ppp, area.size() / ppp);
                }
                ctx.request_repaint_after(Duration::from_millis(16));
            }
            return;
        }
        if self.on {
            // Moved or resized since (a Snap, a drag off the taskbar): not
            // maximised any more.
            if self.placed.is_some_and(|p| !near(now, p)) {
                self.on = false;
                self.placed = None;
            }
        } else {
            self.restore = Some(now);
            self.before = Some(now);
        }
    }

    /// The window manager's maximised rect, once it is one: `viewport()`
    /// says maximised before the window is (at the first frame it still
    /// carries the size the window was opened at, in the wrong units, #87),
    /// and the window manager resizes before it moves. So it counts only
    /// once it differs from the rect before and lies within the monitor —
    /// or, failing that (a monitor not at the origin), after a while.
    fn maximized_rect(
        &mut self,
        ctx: &egui::Context,
        now: egui::Rect,
        before: egui::Rect,
    ) -> Option<egui::Rect> {
        let since = *self.max_since.get_or_insert_with(Instant::now);
        let ppp = ctx.pixels_per_point();
        let monitor = ctx.input(|i| i.viewport().monitor_size)? * ppp;
        // The monitor where X has it, which under WSLg is not at the origin
        // whenever another sits above or left of it (#540).
        let screen = x_monitors()
            .and_then(|screens| screen_of(now, &screens))
            .unwrap_or(egui::Rect::from_min_size(egui::Pos2::ZERO, monitor))
            .expand(1.0);
        let settled = !near(now, before) && screen.contains_rect(now);
        // Not the far-off rect an unmapped window reports, either.
        let waited = since.elapsed() > GIVE_UP
            && now.width() <= monitor.x + 1.0
            && now.min.x > -monitor.x * 4.0;
        (settled || waited).then_some(now)
    }

    /// Lay the window over `area` (physical pixels): one move, one resize.
    fn lay_over(&mut self, ctx: &egui::Context, area: egui::Rect) {
        let ppp = ctx.pixels_per_point();
        self.on = true;
        self.placed = None;
        self.pending = Some((area, Instant::now()));
        self.place(ctx, area.min / ppp, area.size() / ppp);
        ctx.request_repaint();
    }

    /// The work area of the monitor the window is on, in X's coordinates, if
    /// it is known without asking the window manager: from Windows, or from
    /// last time.
    fn work_area(&self, ctx: &egui::Context) -> Option<egui::Rect> {
        let ppp = ctx.pixels_per_point();
        let (inner, monitor) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().monitor_size));
        let window = px(inner?, ppp);
        let monitor = monitor? * ppp;
        let x = x_monitors();
        let probe = probed(x.as_ref());
        // X's monitors when it says; otherwise where Weston would have put
        // Windows' — and then only one the size winit says the window's is.
        let screen = match (&x, &probe) {
            (Some(screens), _) => screen_of(window, screens),
            (None, Some(probe)) => screen_of(window, &weston_layout(&probe.windows))
                .filter(|s| (s.size() - monitor).length() < 3.0),
            (None, None) => None,
        };
        if let (Some(screen), Some(probe)) = (screen, &probe)
            && let Some(work) = work_area_on(screen, &probe.windows)
        {
            return Some(work);
        }
        // Last time's: only while it is still on the window's monitor, or
        // with no monitors known, on a monitor this size.
        self.learned.filter(|a| match (screen, &x) {
            (Some(screen), Some(_)) => screen.expand(1.0).contains_rect(*a),
            _ => a.width() <= monitor.x + 0.5 && a.height() <= monitor.y + 0.5,
        })
    }

    /// The caption's □, or a double click on it.
    pub fn toggle(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
        } else if self.on {
            self.restore_to(ctx, None);
        } else if let Some(area) = self.work_area(ctx) {
            self.learned = Some(area);
            self.lay_over(ctx, area);
        } else {
            // Not known yet: the window manager's maximise finds it, and
            // `update` takes it from there.
            ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
        }
    }

    /// Back to the size it had, at `corner` (in points) or where it was.
    fn restore_to(&mut self, ctx: &egui::Context, corner: Option<egui::Pos2>) {
        self.on = false;
        self.pending = None;
        self.placed = None;
        let Some(r) = self.restore else {
            return;
        };
        let ppp = ctx.pixels_per_point();
        self.place(ctx, corner.unwrap_or(r.min / ppp), r.size() / ppp);
    }

    /// A drag on the caption of a window maximised by hand: back to its own
    /// size first, under the pointer where it was along the caption, as
    /// Windows does, and then the move.
    fn unmaximize_for_drag(&mut self, ctx: &egui::Context) {
        let ppp = ctx.pixels_per_point();
        let (pointer, inner) = ctx.input(|i| (i.pointer.interact_pos(), i.viewport().inner_rect));
        let (Some(p), Some(inner), Some(r)) = (pointer, inner, self.restore) else {
            self.restore_to(ctx, None);
            return;
        };
        let w = r.width() / ppp;
        let along = (p.x / inner.width().max(1.0)).clamp(0.0, 1.0);
        let corner = egui::pos2(inner.min.x + p.x - w * along, inner.min.y);
        self.restore_to(ctx, Some(corner));
        // The drag moves it from here: a move held back for the shrink
        // would land in the middle of it.
        if let Some((corner, _, _)) = self.deferred_move.take() {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(corner));
        }
    }

    /// Put the window's inner area at `corner`, `size` big, both in points.
    ///
    /// `OuterPosition` though it is the *inner* corner: winit asks X to move
    /// the client window there, and WSLg's Weston takes that as where the
    /// window's content goes, whatever frame extents winit reports for it.
    ///
    /// A move shows at once but a resize only once Giverny has drawn a frame
    /// at the new size, so the two are ordered to hide the gap. Growing, it
    /// moves first: the frame between is the old size already in the corner
    /// it grows from. Shrinking, the move waits until the shrink has landed,
    /// or the frame between would be a screen-sized window pushed half off
    /// the screen.
    fn place(&mut self, ctx: &egui::Context, corner: egui::Pos2, size: egui::Vec2) {
        let now = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size()));
        self.deferred_move = None;
        if now.is_none_or(|now| size.x >= now.x && size.y >= now.y) {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(corner));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        } else {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
            self.deferred_move = Some((corner, size, Instant::now()));
            ctx.request_repaint();
        }
    }
}

/// A rect in points as physical pixels.
fn px(r: egui::Rect, ppp: f32) -> egui::Rect {
    egui::Rect::from_min_max(r.min * ppp, r.max * ppp)
}

/// What Windows said about its monitors: each one's bounds and work area, in
/// Windows' physical pixels, and X's monitors as they were when it was
/// asked, to tell when it needs asking again.
#[derive(Clone)]
struct Probe {
    windows: Vec<(egui::Rect, egui::Rect)>,
    x: Option<Vec<egui::Rect>>,
}

/// The last probe that answered.
static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

/// A probe on its way, so that only one is.
static PROBING: AtomicBool = AtomicBool::new(false);

/// Ask Windows for its monitors' work areas, in the background. PowerShell
/// takes seconds to start, so this is never waited for: until it answers,
/// the saved or the window manager's work area stands in. Done at launch,
/// and again whenever X's monitors are no longer the ones it was done
/// against (see [`probed`]): a screen plugged in, or a switch between
/// duplicate and extend.
pub fn probe_work_areas() {
    if PROBING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("work-areas".into())
        .spawn(|| {
            // X's before Windows': changed in between, the next look sees it
            // and asks again.
            let x = read_x_monitors();
            let out = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", WORK_AREA_PS])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            match out {
                Ok(out) if out.status.success() => {
                    let windows = parse_work_areas(&String::from_utf8_lossy(&out.stdout));
                    tracing::debug!("WSLg work areas: {windows:?}, X monitors: {x:?}");
                    if !windows.is_empty()
                        && let Ok(mut probe) = PROBE.lock()
                    {
                        *probe = Some(Probe { windows, x });
                    }
                }
                Ok(out) => tracing::debug!("work-area probe failed: {}", out.status),
                Err(e) => tracing::debug!("work-area probe failed: {e}"),
            }
            PROBING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        PROBING.store(false, Ordering::Release);
    }
}

/// The last probe, after asking again in the background if `x`, X's
/// monitors now, are not the ones it was made against. Windows' answer
/// still holds meanwhile for every monitor it has: [`work_area_on`] goes by
/// the monitor's own corner, not by where the layout put it.
fn probed(x: Option<&Vec<egui::Rect>>) -> Option<Probe> {
    let probe = PROBE.lock().ok()?.clone()?;
    if x.is_some() && probe.x.as_ref() != x {
        probe_work_areas();
    }
    Some(probe)
}

/// Physical pixels, one monitor a line: bounds, then work area, each
/// `x y w h`. Per-monitor DPI-aware: merely DPI-aware, a monitor at
/// another scale than the main one comes back scaled by the main one's —
/// a 1920×1080 screen at 100 % beside a laptop at 175 % as 3360×1890
/// (#540).
const WORK_AREA_PS: &str = r#"Add-Type -Namespace G -Name D -MemberDefinition '[DllImport("user32.dll")] public static extern System.IntPtr SetThreadDpiAwarenessContext(System.IntPtr c); [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'; try { $ok = [G.D]::SetThreadDpiAwarenessContext([System.IntPtr]::new(-4)) -ne [System.IntPtr]::Zero } catch { $ok = $false }; if (-not $ok) { [void][G.D]::SetProcessDPIAware() }; Add-Type -AssemblyName System.Windows.Forms; foreach ($s in [System.Windows.Forms.Screen]::AllScreens) { $b = $s.Bounds; $w = $s.WorkingArea; "$($b.X) $($b.Y) $($b.Width) $($b.Height) $($w.X) $($w.Y) $($w.Width) $($w.Height)" }"#;

/// The work area, in X's coordinates, of the X monitor at `screen`, from
/// `windows`: each Windows monitor's bounds and work area, in Windows'
/// pixels.
///
/// Under WSLg the two are not the same coordinates (#540). Weston lays
/// Windows' monitors out with their combined top-left at (0, 0), so a
/// screen placed above or left of the main one moves every other one down
/// or right in X: a laptop at Windows' (0, 0) beside a monitor set 628 px
/// higher is at X's (0, 628). And Weston lays them out again only when a
/// monitor is added or changes, not when one goes, so after a switch to
/// duplicate, or unplugging, the laptop stays where the layout before had
/// it — which no sum over Windows' monitors can know.
///
/// So the work area goes by the monitor itself: the Windows monitor that is
/// `screen` is the one its size, the nearest to where Weston would have put
/// it, and its work area keeps the same place on it.
fn work_area_on(screen: egui::Rect, windows: &[(egui::Rect, egui::Rect)]) -> Option<egui::Rect> {
    let corner = desktop_corner(windows);
    let off = |bounds: &egui::Rect| ((bounds.min - corner) - screen.min.to_vec2()).length();
    let (bounds, work) = windows
        .iter()
        .filter(|(bounds, _)| (bounds.size() - screen.size()).length() < 3.0)
        .min_by(|(a, _), (b, _)| off(a).total_cmp(&off(b)))?;
    Some(work.translate(screen.min - bounds.min))
}

/// The top-left of Windows' monitors together, as Weston finds it: never
/// right of or below the main monitor's corner, (0, 0).
fn desktop_corner(windows: &[(egui::Rect, egui::Rect)]) -> egui::Pos2 {
    windows
        .iter()
        .fold(egui::Pos2::ZERO, |c, (bounds, _)| c.min(bounds.min))
}

/// Where Weston puts Windows' monitors in X, for when X will not say.
fn weston_layout(windows: &[(egui::Rect, egui::Rect)]) -> Vec<egui::Rect> {
    let corner = desktop_corner(windows).to_vec2();
    windows
        .iter()
        .map(|(bounds, _)| bounds.translate(-corner))
        .collect()
}

/// The monitor among `screens` a window at `window` is on: the one it
/// covers most of, or, off every one, the nearest.
fn screen_of(window: egui::Rect, screens: &[egui::Rect]) -> Option<egui::Rect> {
    let overlap = |s: &egui::Rect| {
        let i = s.intersect(window);
        if i.is_positive() { i.area() } else { 0.0 }
    };
    let distance = |s: &egui::Rect| s.distance_to_pos(window.center());
    screens.iter().copied().max_by(|a, b| {
        overlap(a)
            .total_cmp(&overlap(b))
            .then_with(|| distance(b).total_cmp(&distance(a)))
    })
}

/// Whether a saved work area (`[x, y, w, h]`, physical pixels) still lies
/// on one of X's monitors — so not one saved before #540 in Windows'
/// coordinates, or on a monitor since unplugged. True when X will not say.
pub fn on_a_screen([x, y, w, h]: [f32; 4]) -> bool {
    let area = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
    x_monitors().is_none_or(|screens| screens.iter().any(|s| s.expand(1.0).contains_rect(area)))
}

/// X's monitors, in physical pixels, sorted: RandR's, which under WSLg
/// are Weston's outputs where Weston put them. Kept for a second, as one
/// maximise asks a few times over.
fn x_monitors() -> Option<Vec<egui::Rect>> {
    static LAST: Mutex<Option<(Instant, Option<Vec<egui::Rect>>)>> = Mutex::new(None);
    let mut last = LAST.lock().ok()?;
    if let Some((at, screens)) = &*last
        && at.elapsed() < Duration::from_secs(1)
    {
        return screens.clone();
    }
    let screens = read_x_monitors();
    *last = Some((Instant::now(), screens.clone()));
    screens
}

/// Ask X for its monitors, on a connection of its own; `None` where there
/// is no X, or no RandR 1.5 on it.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn read_x_monitors() -> Option<Vec<egui::Rect>> {
    use x11_dl::{xlib, xrandr};
    let xlib = xlib::Xlib::open().ok()?;
    let xrandr = xrandr::Xrandr::open().ok()?;
    // SAFETY: a display of its own, opened, used and closed right here; the
    // monitor list is read before it is freed.
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return None;
        }
        let (mut event, mut error, mut major, mut minor) = (0, 0, 0, 0);
        let mut screens = None;
        // Asked of a server without RandR 1.5, `XRRGetMonitors` is an X
        // error, which by default ends the process.
        if (xrandr.XRRQueryExtension)(display, &mut event, &mut error) != 0
            && (xrandr.XRRQueryVersion)(display, &mut major, &mut minor) != 0
            && (major, minor) >= (1, 5)
        {
            let root = (xlib.XDefaultRootWindow)(display);
            let mut n = 0;
            let list = (xrandr.XRRGetMonitors)(display, root, xlib::True, &mut n);
            if !list.is_null() {
                let mut found: Vec<egui::Rect> =
                    std::slice::from_raw_parts(list, usize::try_from(n).unwrap_or(0))
                        .iter()
                        .map(|m| {
                            egui::Rect::from_min_size(
                                egui::pos2(m.x as f32, m.y as f32),
                                egui::vec2(m.width as f32, m.height as f32),
                            )
                        })
                        .filter(|r| r.width() > 0.0 && r.height() > 0.0)
                        .collect();
                (xrandr.XRRFreeMonitors)(list);
                found.sort_by(|a, b| {
                    a.min
                        .x
                        .total_cmp(&b.min.x)
                        .then(a.min.y.total_cmp(&b.min.y))
                });
                screens = (!found.is_empty()).then_some(found);
            }
        }
        (xlib.XCloseDisplay)(display);
        screens
    }
}

#[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
fn read_x_monitors() -> Option<Vec<egui::Rect>> {
    None
}

fn parse_work_areas(out: &str) -> Vec<(egui::Rect, egui::Rect)> {
    out.lines()
        .filter_map(|line| {
            let n: Vec<f32> = line
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let rect = |i: usize| {
                egui::Rect::from_min_size(
                    egui::pos2(n[i], n[i + 1]),
                    egui::vec2(n[i + 2], n[i + 3]),
                )
            };
            (n.len() == 8 && n[2] > 0.0 && n[3] > 0.0 && n[6] > 0.0 && n[7] > 0.0)
                .then(|| (rect(0), rect(4)))
        })
        .collect()
}

/// Two rects within a pixel or two of each other.
fn near(a: egui::Rect, b: egui::Rect) -> bool {
    (a.min - b.min).length() < 2.5 && (a.max - b.max).length() < 2.5
}

/// Draw the caption across the top of the window.
pub fn show(ui: &mut egui::Ui, title: &str, chrome: &Chrome, max: &mut Maximize) {
    let ctx = ui.ctx().clone();
    let maximized = max.is_on(&ctx);
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));

    let rect = ui.max_rect();
    let bar = ui.interact(rect, ui.id().with("caption"), Sense::click_and_drag());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, chrome.panel);
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, chrome.dim.gamma_multiply(0.25)),
    );

    let fg = if focused { chrome.fg } else { chrome.dim };
    // The title where Windows puts it: at the left, after a small mark.
    let mark = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        egui::vec2(8.0, 8.0),
    );
    painter.circle_filled(
        mark.center(),
        4.0,
        if focused { chrome.accent } else { chrome.dim },
    );
    painter.text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        giverny_term::render::bidi::egui_line(title, None),
        egui::FontId::proportional(12.5),
        fg,
    );

    // Minimise, maximise/restore and close, right to left.
    let mut x = rect.right();
    let mut button = |kind: Button| -> bool {
        let r = egui::Rect::from_min_max(
            egui::pos2(x - BUTTON_W, rect.top()),
            egui::pos2(x, rect.bottom()),
        );
        x -= BUTTON_W;
        let resp = ui.interact(r, ui.id().with(kind as u8), Sense::click());
        let (bg, glyph) = match (kind, resp.hovered(), resp.is_pointer_button_down_on()) {
            (Button::Close, true, _) => (Some(CLOSE_RED), Color32::WHITE),
            (_, true, true) => (Some(chrome.fg.gamma_multiply(0.14)), chrome.fg),
            (_, true, false) => (Some(chrome.fg.gamma_multiply(0.08)), chrome.fg),
            _ => (None, fg),
        };
        let painter = ui.painter();
        if let Some(bg) = bg {
            painter.rect_filled(r, 0.0, bg);
        }
        draw_glyph(painter, r.center(), kind, maximized, glyph);
        resp.clicked()
    };
    if button(Button::Close) {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
    if button(Button::Maximize) {
        max.toggle(&ctx);
    }
    if button(Button::Minimize) {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    // The rest of the bar moves the window; a double click maximises, as
    // everywhere on Windows. A press on a resize edge is not a move.
    if bar.double_clicked() {
        max.toggle(&ctx);
    } else if bar.drag_started_by(egui::PointerButton::Primary)
        && edge_under_pointer(&ctx, maximized).is_none()
    {
        if max.on {
            max.unmaximize_for_drag(&ctx);
        }
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Button {
    Minimize,
    Maximize,
    Close,
}

/// The Segoe Fluent caption glyphs, drawn: 10 px, 1 px strokes.
fn draw_glyph(p: &egui::Painter, c: egui::Pos2, kind: Button, maximized: bool, color: Color32) {
    let s = Stroke::new(1.0, color);
    let h = 5.0;
    match kind {
        Button::Minimize => {
            p.hline(c.x - h..=c.x + h, c.y, s);
        }
        Button::Maximize if maximized => {
            // Restore: a square with another behind it.
            let front = egui::Rect::from_min_max(
                egui::pos2(c.x - h, c.y - h + 2.0),
                egui::pos2(c.x + h - 2.0, c.y + h),
            );
            p.rect_stroke(front, 1.0, s, egui::StrokeKind::Middle);
            p.hline(c.x - h + 2.0..=c.x + h, c.y - h, s);
            p.vline(c.x + h, c.y - h..=c.y + h - 2.0, s);
        }
        Button::Maximize => {
            let r = egui::Rect::from_center_size(c, egui::vec2(2.0 * h, 2.0 * h));
            p.rect_stroke(r, 1.0, s, egui::StrokeKind::Middle);
        }
        Button::Close => {
            p.line_segment([c + egui::vec2(-h, -h), c + egui::vec2(h, h)], s);
            p.line_segment([c + egui::vec2(-h, h), c + egui::vec2(h, -h)], s);
        }
    }
}

/// The window edge the pointer is on, if it is within [`EDGE`] of one.
fn edge_under_pointer(ctx: &egui::Context, maximized: bool) -> Option<ResizeDirection> {
    if maximized {
        return None;
    }
    let pos = ctx.input(|i| i.pointer.latest_pos())?;
    edge_at(ctx.content_rect(), pos, EDGE)
}

/// Which edge or corner of `rect` a point within `edge` of it is on.
fn edge_at(rect: egui::Rect, pos: egui::Pos2, edge: f32) -> Option<ResizeDirection> {
    // Corners are generous, as on Windows: a corner's grip runs a little way
    // along both edges, or it would be a few pixels nobody can hit.
    let corner = edge * 3.0;
    let left = pos.x - rect.left() < edge;
    let right = rect.right() - pos.x < edge;
    let top = pos.y - rect.top() < edge;
    let bottom = rect.bottom() - pos.y < edge;
    let near_left = pos.x - rect.left() < corner;
    let near_right = rect.right() - pos.x < corner;
    let near_top = pos.y - rect.top() < corner;
    let near_bottom = rect.bottom() - pos.y < corner;
    Some(match () {
        _ if (top && near_left) || (left && near_top) => ResizeDirection::NorthWest,
        _ if (top && near_right) || (right && near_top) => ResizeDirection::NorthEast,
        _ if (bottom && near_left) || (left && near_bottom) => ResizeDirection::SouthWest,
        _ if (bottom && near_right) || (right && near_bottom) => ResizeDirection::SouthEast,
        _ if top => ResizeDirection::North,
        _ if bottom => ResizeDirection::South,
        _ if left => ResizeDirection::West,
        _ if right => ResizeDirection::East,
        _ => return None,
    })
}

/// A one-pixel outline round the window, as Windows 11 draws one: without a
/// frame a dark window on a dark desktop has no edge at all. Not when
/// maximised, where there is nothing to be told apart from.
pub fn outline(ctx: &egui::Context, chrome: &Chrome, maximized: bool) {
    if maximized {
        return;
    }
    let px = 1.0 / ctx.pixels_per_point();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("window-outline"),
    ));
    painter.rect_stroke(
        ctx.content_rect(),
        0.0,
        Stroke::new(px, chrome.dim.gamma_multiply(0.45)),
        egui::StrokeKind::Inside,
    );
}

/// Turn the window's outermost few points into resize handles. Runs before
/// anything else is drawn, so a press on an edge is the resize's and not the
/// widget's underneath; the cursor shows which way it will go.
pub fn resize_edges(ctx: &egui::Context, maximized: bool) {
    let Some(dir) = edge_under_pointer(ctx, maximized) else {
        return;
    };
    ctx.set_cursor_icon(match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    #[test]
    fn the_middle_is_no_edge() {
        assert_eq!(edge_at(rect(), egui::pos2(400.0, 300.0), 5.0), None);
        assert_eq!(edge_at(rect(), egui::pos2(6.0, 300.0), 5.0), None);
    }

    #[test]
    fn edges_and_corners() {
        let r = rect();
        assert_eq!(
            edge_at(r, egui::pos2(2.0, 300.0), 5.0),
            Some(ResizeDirection::West)
        );
        assert_eq!(
            edge_at(r, egui::pos2(798.0, 300.0), 5.0),
            Some(ResizeDirection::East)
        );
        assert_eq!(
            edge_at(r, egui::pos2(400.0, 1.0), 5.0),
            Some(ResizeDirection::North)
        );
        assert_eq!(
            edge_at(r, egui::pos2(400.0, 599.0), 5.0),
            Some(ResizeDirection::South)
        );
        // A corner's grip runs a little way along each edge.
        assert_eq!(
            edge_at(r, egui::pos2(10.0, 1.0), 5.0),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            edge_at(r, egui::pos2(1.0, 10.0), 5.0),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            edge_at(r, egui::pos2(790.0, 598.0), 5.0),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            edge_at(r, egui::pos2(799.0, 2.0), 5.0),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            edge_at(r, egui::pos2(3.0, 595.0), 5.0),
            Some(ResizeDirection::SouthWest)
        );
    }

    #[test]
    fn work_areas_are_read_a_monitor_a_line() {
        let out = "0 0 2880 1800 0 0 2880 1716\r\n-1920 0 1920 1080 -1920 0 1920 1040\r\nnonsense\r\n0 0 0 0 0 0 0 0\r\n";
        let areas = parse_work_areas(out);
        assert_eq!(areas.len(), 2);
        assert_eq!(
            areas[0].1,
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2880.0, 1716.0))
        );
        assert_eq!(areas[1].0.min, egui::pos2(-1920.0, 0.0));
        assert_eq!(areas[1].1.height(), 1040.0);
    }

    fn r(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    /// A 2880×1800 laptop and a 1920×1080 screen right of it and 628 px
    /// higher, as Windows has them (#540).
    fn laptop_and_screen() -> Vec<(egui::Rect, egui::Rect)> {
        vec![
            (r(0.0, 0.0, 2880.0, 1800.0), r(0.0, 0.0, 2880.0, 1716.0)),
            (
                r(2880.0, -628.0, 1920.0, 1080.0),
                r(2880.0, -628.0, 1920.0, 1032.0),
            ),
        ]
    }

    #[test]
    fn a_work_area_moves_with_its_monitor_into_x() {
        let windows = laptop_and_screen();
        // Where WSLg's Weston put them, as RandR reported it.
        let laptop = r(0.0, 628.0, 2880.0, 1800.0);
        let screen = r(2880.0, 0.0, 1920.0, 1080.0);
        assert_eq!(weston_layout(&windows), vec![laptop, screen]);
        assert_eq!(
            work_area_on(laptop, &windows),
            Some(r(0.0, 628.0, 2880.0, 1716.0))
        );
        assert_eq!(
            work_area_on(screen, &windows),
            Some(r(2880.0, 0.0, 1920.0, 1032.0))
        );
    }

    #[test]
    fn a_monitor_weston_left_behind_is_found_by_its_size() {
        // Duplicate: Windows has the laptop alone, and Weston left it where
        // the extended layout had it.
        let windows = vec![laptop_and_screen()[0]];
        assert_eq!(
            work_area_on(r(0.0, 628.0, 2880.0, 1800.0), &windows),
            Some(r(0.0, 628.0, 2880.0, 1716.0))
        );
        // A monitor Windows did not tell of has no work area here.
        assert_eq!(work_area_on(r(2880.0, 0.0, 1920.0, 1080.0), &windows), None);
    }

    #[test]
    fn two_monitors_the_same_size_go_by_place() {
        let windows = vec![
            (r(0.0, 0.0, 1920.0, 1080.0), r(0.0, 0.0, 1920.0, 1040.0)),
            (
                r(-1920.0, 0.0, 1920.0, 1080.0),
                r(-1920.0, 0.0, 1920.0, 1080.0),
            ),
        ];
        assert_eq!(
            work_area_on(r(1920.0, 0.0, 1920.0, 1080.0), &windows),
            Some(r(1920.0, 0.0, 1920.0, 1040.0))
        );
        assert_eq!(
            work_area_on(r(0.0, 0.0, 1920.0, 1080.0), &windows),
            Some(r(0.0, 0.0, 1920.0, 1080.0))
        );
    }

    #[test]
    fn the_window_is_on_the_monitor_it_covers_most() {
        let screens = [
            r(0.0, 628.0, 2880.0, 1800.0),
            r(2880.0, 0.0, 1920.0, 1080.0),
        ];
        // Laid over Windows' work area as if it were X's: still mostly on
        // the laptop.
        assert_eq!(
            screen_of(r(0.0, 0.0, 2880.0, 1716.0), &screens),
            Some(screens[0])
        );
        assert_eq!(
            screen_of(r(3000.0, 100.0, 800.0, 600.0), &screens),
            Some(screens[1])
        );
        // Off both: the nearer.
        assert_eq!(
            screen_of(r(100.0, 50.0, 400.0, 300.0), &screens),
            Some(screens[0])
        );
    }

    #[test]
    fn near_is_a_pixel_or_two() {
        let r = rect();
        assert!(near(r, r.translate(egui::vec2(1.0, -1.0))));
        assert!(!near(r, r.translate(egui::vec2(32.0, 32.0))));
        assert!(!near(r, r.expand(4.0)));
    }
}
