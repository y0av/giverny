//! Giverny — a native terminal built around Claude Code.

mod capture;
mod chrome;
mod claude_watch;
mod desktop;
mod icon;
mod keymap;
mod oom;
mod overlays;
mod prompt_bar;
mod rail;
mod settings_ui;
mod splash;
mod taskbar;
mod titlebar;
mod translucency;
mod update;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod wayland_dnd;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod wslg;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod wslg_cursor;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Modifiers};
use giverny_claude::wsl;
use giverny_core::config;
use giverny_core::history;
use giverny_core::state::{self, Paths, SaveState};
use giverny_core::tabs::{CategoryId, TabId, Workspace};
use giverny_term::proxy::TabEvent;
use giverny_term::pty::{self, GridSize, SpawnCfg};
use giverny_term::render::theme::Theme;
use giverny_term::session::TermSession;
use giverny_term::tee::TeeEvent;
use giverny_term::widget::{DEFAULT_FONT_SIZE, RenderShared, TabView};

/// Where an attached agent's tab belongs.
///
/// Not "whichever category happened to be open", which is how an agent
/// working in one project ended up filed under another. The agent's own
/// directory decides: a category already working there takes it, then one
/// named after that directory, and failing both there is a new category with
/// that name.
fn category_for_agent(ws: &mut Workspace, cwd: Option<&Path>) -> CategoryId {
    let here = |ws: &Workspace| {
        ws.active_tab()
            .map(|t| t.category)
            .or_else(|| ws.categories.first().map(|c| c.id))
    };
    let Some(cwd) = cwd else {
        return here(ws).unwrap_or_else(|| ws.add_category("agents"));
    };
    if let Some(cat) = ws
        .tabs
        .iter()
        .find(|t| t.cwd.as_deref() == Some(cwd))
        .map(|t| t.category)
    {
        return cat;
    }
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "agents".into());
    if let Some(cat) = ws.categories.iter().find(|c| c.name == name) {
        return cat.id;
    }
    ws.add_category(&name)
}

/// The theme a name means right now. Only Rouen cares when now is: its light
/// follows the local clock.
fn theme_for(name: &str) -> Theme {
    if name == "rouen" {
        let now = jiff::Zoned::now();
        return Theme::rouen_at(now.hour() as f32 + now.minute() as f32 / 60.0);
    }
    Theme::by_name(name)
}

/// `outer` less `hole`, as up to four rects that tile it without overlapping:
/// full-width bands above and below, and the sides between them.
fn around(outer: egui::Rect, hole: Option<egui::Rect>) -> Vec<egui::Rect> {
    let Some(hole) = hole.map(|h| h.intersect(outer)).filter(|h| h.is_positive()) else {
        return vec![outer];
    };
    let x = outer.x_range();
    [
        egui::Rect::from_x_y_ranges(x, outer.top()..=hole.top()),
        egui::Rect::from_x_y_ranges(x, hole.bottom()..=outer.bottom()),
        egui::Rect::from_x_y_ranges(outer.left()..=hole.left(), hole.y_range()),
        egui::Rect::from_x_y_ranges(hole.right()..=outer.right(), hole.y_range()),
    ]
    .into_iter()
    .filter(|r| r.is_positive())
    .collect()
}

/// The opacity backgrounds are painted at: the configured one in a window
/// that can show it, solid in any other.
fn opacity_for(see_through: bool, cfg: &config::Config) -> f32 {
    if see_through {
        cfg.window.opacity()
    } else {
        1.0
    }
}

/// Remember accounts that only the environment knew about.
///
/// An account kept somewhere unguessable is found through the environment —
/// `CLAUDE_CONFIG_DIR`, or a list in `CCTOP_CONFIG_DIRS` for people who keep
/// one. Both usually come from a shell rc, which means they exist when
/// Giverny starts from a terminal and not when it starts from a launcher, so
/// the account list changed depending on how the app was opened.
///
/// Writing them into the config the first time we see them makes the list
/// ours: from then on it is the same however Giverny was started, and it is
/// visible and editable in settings rather than hidden in a shell rc.
/// Whether dropping a file into a tab can work here.
///
/// Two different paths, and it is worth saying which one is in play: winit
/// delivers drops on X11, Windows and macOS, while on Wayland it has no
/// `wl_data_device` at all and Giverny reads the drags itself (see
/// `wayland_dnd`). Only the Wayland path knows where the pointer is, so only
/// it can aim a drop at a particular tab.
fn drag_drop_status() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        println!("file drag-and-drop  available (Wayland, via the clipboard data device)");
        println!("      a drop lands in the tab under the pointer.");
    } else {
        println!("file drag-and-drop  available (X11/Windows/macOS, via winit)");
        println!("      a drop lands in the active tab: winit reports no drag position.");
    }
    println!();
}

fn remember_env_accounts(paths: &Paths, cfg: &mut config::Config) {
    let mut from_env: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        from_env.push(PathBuf::from(dir));
    }
    if let Ok(list) = std::env::var("CCTOP_CONFIG_DIRS") {
        from_env.extend(list.split(':').filter(|p| !p.is_empty()).map(PathBuf::from));
    }

    // Anything found without being told stays out of the config — no point
    // recording ~/.claude or a dir the scan picks up anyway. This must not
    // consult the environment: the environment is what is being decided about,
    // so `discover()` here would find everything and record nothing.
    let found_anyway = giverny_claude::profiles::ambient_dirs();
    let mut merged = cfg.behavior.extra_profile_dirs.clone();
    let mut added = 0;
    for dir in from_env {
        if merged.contains(&dir)
            || found_anyway.contains(&dir)
            || !giverny_claude::profiles::looks_like_account(&dir)
        {
            continue;
        }
        merged.push(dir);
        added += 1;
    }
    if added == 0 {
        return;
    }
    let Some(def) = giverny_core::settings::by_key("behavior.extra_profile_dirs") else {
        return;
    };
    let value = giverny_core::settings::Value::List(
        merged.iter().map(|p| p.display().to_string()).collect(),
    );
    match giverny_core::settings::write(paths.base(), def, &value) {
        Ok(()) => {
            tracing::info!("remembered {added} account dir(s) from the environment");
            cfg.behavior.extra_profile_dirs = merged;
        }
        Err(err) => tracing::warn!("could not record account dirs: {err:#}"),
    }
}

/// Claude Code marks its own subprocesses with session-identity variables.
/// Launch Giverny *from* a Claude session — a tab, or `claude` in any
/// terminal — and those markers are inherited, then handed to every shell we
/// spawn: the `claude` inside each tab decides it is a child session and
/// turns off transcript saving, which silently disables resume.
///
/// Scrub them before any thread exists (the only safe window to mutate the
/// environment). `CLAUDE_CONFIG_DIR` is deliberately kept — that is account
/// selection, not session identity — as are `ANTHROPIC_*` credentials-ish
/// and provider settings we have no business touching.
/// Which renderer to open with.
///
/// wgpu wants Vulkan, Metal or DX12. A machine with none of them — an Intel
/// GPU from before 2013, a driver nobody has updated since — gets no window at
/// all: wgpu finds only a CPU adapter, fails to configure a surface, and
/// panics with a validation error that means nothing to whoever is reading it.
/// OpenGL is older than all of that and those drivers do have it, so it is the
/// fallback, and nothing in this app knows the difference: every pixel is an
/// egui mesh or texture either way.
///
/// The same goes for a machine where wgpu *would* work but only on a CPU
/// adapter (lavapipe, llvmpipe, WARP): WSLg with no GPU Vulkan driver is the
/// common case, where OpenGL reaches the real GPU through Mesa's d3d12 driver
/// and wgpu draws every frame in software at several cores' worth of CPU. So
/// unless told otherwise, ask wgpu which adapter it would pick first.
///
/// `GIVERNY_RENDERER=glow|wgpu` decides instead, which is also how the retry
/// below re-launches itself.
///
/// Also returns the wgpu backends to ask for, where eframe's own choice is the
/// wrong one (see `preferred_backends`).
fn pick_renderer(gpu_gl: bool) -> (eframe::Renderer, Option<eframe::wgpu::Backends>) {
    match std::env::var("GIVERNY_RENDERER").as_deref() {
        Ok("glow" | "gl" | "opengl") => {
            tracing::info!("renderer: OpenGL (GIVERNY_RENDERER)");
            (eframe::Renderer::Glow, None)
        }
        Ok("wgpu") => (eframe::Renderer::Wgpu, preferred_backends()),
        // OpenGL is known to reach the GPU; wgpu is not asked, because on
        // WSLg its only Vulkan driver is lavapipe (see `wslg`).
        _ if gpu_gl => (eframe::Renderer::Glow, None),
        _ => {
            // The preferred backends only if they reach a GPU; otherwise
            // whatever eframe would have picked.
            let preferred = preferred_backends().and_then(|backends| {
                wgpu_adapter(Some(backends))
                    .filter(|info| info.device_type != eframe::wgpu::DeviceType::Cpu)
                    .map(|info| (backends, info))
            });
            let (backends, adapter) = match preferred {
                Some((backends, info)) => (Some(backends), Some(info)),
                None => (None, wgpu_adapter(None)),
            };
            let renderer = renderer_for(adapter.as_ref());
            match (&adapter, renderer) {
                (None, _) => tracing::info!("renderer: OpenGL, wgpu found no adapter"),
                (Some(info), eframe::Renderer::Glow) => tracing::info!(
                    "renderer: OpenGL, wgpu would draw on the CPU ({}, {:?})",
                    info.name,
                    info.backend
                ),
                (Some(info), _) => tracing::debug!(
                    "renderer: wgpu on {} ({:?}, {:?})",
                    info.name,
                    info.backend,
                    info.device_type
                ),
            }
            (renderer, backends)
        }
    }
}

/// Whether OpenGL has been pointed at a GPU that wgpu cannot reach: WSLg's
/// d3d12 driver. Logs the outcome either way, since it decides the CPU cost.
fn gpu_opengl() -> bool {
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    match wslg::enable_d3d12() {
        wslg::Gpu::NotApplicable => {}
        wslg::Gpu::D3d12(renderer) => {
            tracing::info!("renderer: OpenGL on the GPU through WSLg's d3d12 driver ({renderer})");
            return true;
        }
        wslg::Gpu::Unavailable(why) => {
            tracing::warn!(
                "WSLg's d3d12 GL driver is unavailable, drawing may fall back to the CPU: {why}"
            );
        }
    }
    false
}

/// The wgpu backends to draw with where eframe's default is the wrong one,
/// `None` where it is right.
///
/// On Windows wgpu lists Vulkan before DirectX 12, and eframe takes the first
/// GPU adapter it finds. Intel's Vulkan driver presents a window badly: it
/// flickers as the mouse moves over it, after it opens and again after it has
/// been hidden for a few minutes, then settles. DirectX 12 is Windows'
/// own path to the screen and does not. `WGPU_BACKEND` still decides.
fn preferred_backends() -> Option<eframe::wgpu::Backends> {
    preferred_backends_for(cfg!(windows), std::env::var_os("WGPU_BACKEND").is_some())
}

fn preferred_backends_for(windows: bool, env_chose: bool) -> Option<eframe::wgpu::Backends> {
    (windows && !env_chose).then_some(eframe::wgpu::Backends::DX12)
}

/// eframe's default wgpu setup, on `backends` instead of its own if given.
fn wgpu_setup(backends: Option<eframe::wgpu::Backends>) -> eframe::egui_wgpu::WgpuSetupCreateNew {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    if let Some(backends) = backends {
        setup.instance_descriptor.backends = backends;
    }
    setup
}

/// The adapter that wgpu setup would choose, asked for the same way (same
/// backends, same power preference) but without a window yet.
fn wgpu_adapter(backends: Option<eframe::wgpu::Backends>) -> Option<eframe::wgpu::AdapterInfo> {
    let setup = wgpu_setup(backends);
    let instance = eframe::wgpu::Instance::new(setup.instance_descriptor);
    let options = eframe::wgpu::RequestAdapterOptions {
        power_preference: setup.power_preference,
        compatible_surface: None,
        force_fallback_adapter: false,
    };
    pollster::block_on(instance.request_adapter(&options))
        .ok()
        .map(|adapter| adapter.get_info())
}

/// Does the window draw on a GPU? Asked of the renderer the window actually
/// got, not the one we hoped for: the rail's spinners turn smoothly only
/// where a frame is cheap (#70), and a software rasterizer is where it is not.
fn draws_on_gpu(cc: &eframe::CreationContext<'_>) -> bool {
    if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext;
        let renderer = unsafe { gl.get_parameter_string(eframe::glow::RENDERER) };
        let gpu = !is_software_gl(&renderer);
        tracing::info!(
            "frames {}: OpenGL renderer {renderer}",
            if gpu { "cheap" } else { "on the CPU" }
        );
        return gpu;
    }
    if let Some(wgpu) = &cc.wgpu_render_state {
        return wgpu.adapter.get_info().device_type != eframe::wgpu::DeviceType::Cpu;
    }
    false
}

/// Mesa's and Windows' CPU rasterizers, by the renderer string they report.
fn is_software_gl(renderer: &str) -> bool {
    let r = renderer.to_ascii_lowercase();
    [
        "llvmpipe",
        "softpipe",
        "lavapipe",
        "swrast",
        "software",
        "gdi generic",
    ]
    .iter()
    .any(|s| r.contains(s))
}

/// wgpu unless its adapter is missing or a software rasterizer, where OpenGL
/// is the better bet: it either reaches a GPU wgpu could not, or is no slower.
fn renderer_for(adapter: Option<&eframe::wgpu::AdapterInfo>) -> eframe::Renderer {
    match adapter {
        Some(info) if info.device_type != eframe::wgpu::DeviceType::Cpu => eframe::Renderer::Wgpu,
        _ => eframe::Renderer::Glow,
    }
}

/// Start again on OpenGL, once, after wgpu failed to put a window on screen.
///
/// Called from the panic hook, which is the last moment this process is still
/// able to do anything: what follows it is an abort. The child inherits the
/// arguments and is told which renderer to use, so it cannot arrive back here.
fn restart_on_opengl() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| "giverny".into());
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("GIVERNY_RENDERER", "glow")
        .status();
    std::process::exit(match status {
        Ok(s) => s.code().unwrap_or(0),
        Err(_) => 1,
    });
}

/// Let go of a console Windows gave us, but never one we were launched from.
///
/// The binary is console-subsystem, because `giverny doctor` and the hook
/// entrypoints have to be able to print. The cost is that opening the app from
/// the Start menu or a shortcut also opens a console window beside it, full of
/// log lines, which cannot be closed without killing the app — reported as
/// "it opens PowerShell in the background and I'm not allowed to close it".
///
/// The two cases are distinguishable: a console Windows created for us has one
/// process attached, and a console we inherited from a shell has at least two.
/// Detaching from ours closes it; detaching from theirs would leave their
/// window alone, but there is no reason to, and every reason to keep printing
/// where they can see it.
#[cfg(windows)]
fn release_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut attached = [0u32; 4];
    // SAFETY: the pointer and length describe the array above.
    let count = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) };
    if count == 1 {
        // SAFETY: no console output is expected after this; the app is a
        // window from here on.
        unsafe { FreeConsole() };
    }
}

#[cfg(not(windows))]
fn release_own_console() {}

fn scrub_inherited_claude_markers() {
    const MARKERS: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "CLAUDE_CODE_SESSION_NAME",
        "CLAUDE_CODE_SESSION_KIND",
        "CLAUDE_CODE_JSONL_TRANSCRIPT",
        "CLAUDE_CODE_SESSION_LOG",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE",
        "CLAUDE_PROJECT_DIR",
        "CLAUDE_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_DATA",
        "CLAUDE_EFFORT",
    ];
    let inherited: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| MARKERS.contains(&k.as_str()) || k.starts_with("CLAUDE_BG_"))
        .collect();
    if inherited.is_empty() {
        return;
    }
    for key in &inherited {
        // SAFETY: single-threaded here — called first thing in main.
        unsafe { std::env::remove_var(key) };
    }
    eprintln!(
        "giverny: cleared {} inherited Claude session marker(s) so tabs start clean",
        inherited.len()
    );
}

/// `giverny --help`, and what an unknown command prints to stderr.
const USAGE: &str = "giverny — a native terminal built around Claude Code\n\n\
     USAGE:\n  giverny            launch the terminal\n  \
     giverny doctor     diagnose Claude integration\n  \
     giverny welcome [from-version]\n                     \
     print the welcome screen\n  \
     giverny update     check for a newer release\n  \
     giverny install-desktop [--remove]\n                     \
     install the desktop entry + icons (needed for the\n                     \
     taskbar icon on Wayland)\n  \
     giverny relay      (internal) Claude Code hook entrypoint\n  \
     giverny statusline (internal) Claude Code statusline entrypoint\n\n\
     FLAGS:\n  -V, --version  print the version\n  \
     -h, --help     print this help";

/// Is `arg`, the first argument, a command this build does not have?
///
/// The window takes no arguments, so anything past the known subcommands
/// used to be ignored and the window opened anyway. A bare word is refused:
/// it is a command, likely one newer than this binary. Left to open the
/// window as before: flags (a launcher may add some, e.g. macOS's `-psn_…`),
/// and anything that reads as a path — one that `exists`, or is spelled with
/// a separator, `.` or `~` — in case a launcher or file manager hands one over.
fn is_unknown_subcommand(arg: &str, exists: bool) -> bool {
    !arg.is_empty() && !arg.starts_with(['-', '.', '~']) && !arg.contains(['/', '\\']) && !exists
}

fn main() -> eframe::Result {
    scrub_inherited_claude_markers();

    // Subcommands that never open a window.
    match std::env::args().nth(1).as_deref() {
        Some("relay") => {
            giverny_claude::hooks::run_relay(&Paths::default_dirs().hook_spool());
            return Ok(());
        }
        Some("statusline") => {
            giverny_claude::hooks::run_statusline(&Paths::default_dirs().hook_spool());
            return Ok(());
        }
        Some("welcome") => {
            let arg = std::env::args().nth(2);
            print!(
                "{}",
                splash::render(&match arg.as_deref() {
                    Some("first") | None => splash::Welcome::First,
                    Some(from) => splash::Welcome::Updated {
                        from: Some(from.to_string()),
                    },
                })
            );
            return Ok(());
        }
        Some("doctor") => {
            doctor();
            return Ok(());
        }
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
        Some(wslg::PROBE_ARG) => std::process::exit(wslg::probe()),
        Some("update") => {
            update_cli();
            return Ok(());
        }
        // Wayland cannot take an icon from the client; it reads one from an
        // installed desktop entry matched on app_id. See `desktop`.
        Some("install-desktop") => {
            match std::env::args().nth(2).as_deref() {
                Some("--remove") => desktop::remove(),
                _ => desktop::install(),
            }
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("giverny {}", update::CURRENT);
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        // A word this build does not know: a typo, or a subcommand added
        // since it was built. It must not fall through to opening a window,
        // which would also set up the Claude accounts from this binary.
        Some(arg) if is_unknown_subcommand(arg, Path::new(arg).exists()) => {
            eprintln!("giverny: unknown command '{arg}'\n\n{USAGE}");
            std::process::exit(2);
        }
        _ => {}
    }

    // Past every subcommand: what follows is the window, and a console that
    // exists only because Windows made one has nothing left to show.
    release_own_console();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giverny=info,warn".into()),
        )
        .init();

    if oom::at_risk() {
        tracing::warn!(
            "this scope stops on an OOM kill: one tab's process being killed \
             would close every tab — run `giverny install-desktop` to fix it"
        );
    }

    // Backend choice has to happen before the event loop exists. winit picks
    // Wayland whenever WAYLAND_DISPLAY is set, and has no drop support there,
    // so this is the switch that buys file drag-and-drop.
    let paths = Paths::default_dirs();
    // `GIVERNY_NO_X11` is set by the fallback below, so a second attempt
    // cannot loop.
    let launch_cfg = config::load(paths.base());
    let prefer_x11 = launch_cfg.behavior.prefer_x11;
    // WSLg's compositor crashes under our Wayland window (see `wslg`).
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    let wslg_x11 = !prefer_x11 && wslg::avoid_wayland();
    #[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
    let wslg_x11 = false;
    let try_x11 = (prefer_x11 || wslg_x11)
        && std::env::var_os("GIVERNY_NO_X11").is_none()
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut wayland_stashed = None;
    if try_x11 {
        wayland_stashed = std::env::var_os("WAYLAND_DISPLAY");
        // SAFETY: no threads yet — this runs before the event loop.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
        if wslg_x11 {
            tracing::info!(
                "WSLg: using X11/XWayland, since WSLg's compositor crashes under \
                 Wayland windows (GIVERNY_WAYLAND=1 to use Wayland anyway)"
            );
        } else {
            tracing::info!("prefer_x11: using X11/XWayland so file drops arrive");
        }
    }

    // Reopen at the size the user left it. Read before the window exists, so
    // it can't be applied as a resize the user sees happen.
    let gpu_gl = gpu_opengl();
    let (renderer, wgpu_backends) = pick_renderer(gpu_gl);
    let layout = state::load_layout(&paths);
    // The interface zoom the user last left, or — on a first run only — the
    // display's own scale where the platform hides it from winit (#62). Not
    // on a later run that simply predates saving the zoom: its terminal font
    // was already sized to make up for the 1.0, and would come out huge.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    let display_zoom = (try_x11 && wslg_x11 && !paths.state_file().exists())
        .then(wslg::desktop_scale)
        .flatten();
    #[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
    let display_zoom: Option<f32> = None;
    if let Some(z) = display_zoom {
        tracing::info!("WSLg: display scale {z} is not visible to X11 clients; zooming to match");
    }
    let zoom = layout.zoom().or(display_zoom);
    // A first window scaled with its contents, so they have the room the
    // default size was chosen for.
    let first_size = [1280.0, 820.0].map(|v| v * display_zoom.unwrap_or(1.0));
    // WSLg frames an X11 window with Weston's own caption — a 1x-scale strip
    // with Windows-95 buttons inside a thick black band — and there is no way
    // to have Windows draw one instead, so there Giverny draws its own (#69).
    // `GIVERNY_WSLG_FRAME=1` keeps Weston's.
    let frameless = try_x11 && wslg_x11 && std::env::var_os("GIVERNY_WSLG_FRAME").is_none();
    // That window is maximised by hand (#78): straight over the work area
    // saved last time when it opens maximised, rather than through the
    // window manager, whose maximise lands it 32 px off.
    let window_size = layout.window_size().unwrap_or(first_size);
    let work_area = layout.work_area.filter(|_| frameless);
    let open_laid = frameless && layout.maximized && work_area.is_some();
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id("giverny")
        .with_title("Giverny")
        .with_icon(icon::icon_data(16))
        .with_inner_size(window_size)
        .with_maximized(layout.maximized && !open_laid)
        .with_min_inner_size([640.0, 400.0])
        .with_decorations(!frameless);
    // Only when asked for: a transparent surface costs a compositor blend
    // every frame, and a solid window should be exactly what it was.
    let see_through = translucency::request(&launch_cfg.window);
    if see_through {
        viewport = viewport.with_transparent(true);
    }
    if let (true, Some([x, y, w, h])) = (open_laid, work_area) {
        viewport = viewport.with_position([x, y]).with_inner_size([w, h]);
    }
    let maximize =
        titlebar::Maximize::new(work_area, frameless && layout.maximized, window_size.into());
    if frameless {
        titlebar::probe_work_areas();
    }
    let mut options = eframe::NativeOptions {
        renderer,
        viewport,
        ..Default::default()
    };
    if let Some(backends) = wgpu_backends {
        options.wgpu_options.wgpu_setup = wgpu_setup(Some(backends)).into();
    }
    // A hook rather than `catch_unwind`: wgpu's failure panics, and then
    // panics again on the way out, and a panic while panicking aborts the
    // process on the spot — there is nothing left to catch. A hook runs
    // first, on the panicking thread, before any of that.
    if renderer == eframe::Renderer::Wgpu && std::env::var_os("GIVERNY_RENDERER").is_none() {
        let inherited = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or_default();
            if message.contains("wgpu") || message.contains("Surface") {
                eprintln!("giverny: the GPU renderer failed; starting again on OpenGL");
                restart_on_opengl();
            }
            inherited(info);
        }));
    }
    let result = eframe::run_native(
        "Giverny",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                zoom,
                frameless,
                maximize,
                see_through,
            )))
        }),
    );

    // No X server after all — no XWayland, or no XAUTHORITY. Preferring
    // drag-and-drop must not be able to leave the app unlaunchable, with the
    // only remedy being to edit a config file you cannot see.
    //
    // Re-exec rather than retry: winit refuses to build a second event loop in
    // one process (`RecreationAttempt`), so the only way back is a fresh one.
    if let Err(err) = &result
        && try_x11
    {
        tracing::warn!("prefer_x11 failed ({err}); restarting on Wayland");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let exe = std::env::current_exe().unwrap_or_else(|_| "giverny".into());
            let mut cmd = std::process::Command::new(exe);
            cmd.args(std::env::args_os().skip(1))
                .env("GIVERNY_NO_X11", "1");
            // The child inherits *this* environment, where WAYLAND_DISPLAY was
            // removed — so it has to be put back explicitly or the child picks
            // X11 again and fails the same way.
            if let Some(wl) = wayland_stashed {
                cmd.env("WAYLAND_DISPLAY", wl);
            }
            let error = cmd.exec();
            tracing::error!("could not restart: {error}");
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Tab(TabId),
    Category(CategoryId),
}

#[derive(Debug, Clone)]
pub enum Action {
    NewTab {
        category: CategoryId,
        cwd: Option<PathBuf>,
    },
    NewCategory,
    CloseTab(TabId),
    Select(TabId),
    Cycle(i32),
    /// Step through recently used tabs (Ctrl+Tab), committing on release.
    SwitchRecent(i32),
    ToggleCollapse(CategoryId),
    StartRename(RenameTarget),
    CommitRename(RenameTarget, Option<String>),
    Respawn(TabId),
    DeleteCategory(CategoryId),
    SetCategoryColor(CategoryId, usize),
    MoveTab(TabId, CategoryId),
    InstallHooks,
    DismissHooksBanner,
    SetCategoryProfile(CategoryId, Option<PathBuf>),
    /// Jump to the next tab where Claude needs attention (then done-unseen).
    JumpAttention,
    TogglePalette,
    ToggleStatusline(bool),
    RefreshUsage,
    OpenSessions(TabId),
    /// Resume a specific past conversation in a tab.
    ResumeSpecific(TabId, String, PathBuf),
    /// Drag-and-drop: place a tab in a category at a position.
    ReorderTab(TabId, CategoryId, usize),
    /// Drag-and-drop: move a category to a position in rail order.
    ReorderCategory(CategoryId, usize),
    /// Run the official install command in a visible tab.
    RunUpdate,
    DismissUpdate,
    /// Hand over to the version the installer just put on disk.
    RestartNow,
    ToggleSettings,
    ToggleKeys,
    /// Write one option back to config.toml and apply it now.
    SetSetting(String, giverny_core::settings::Value),
    /// Open config.toml in $EDITOR, in a tab.
    EditConfig,
    /// Attach a tab to a background agent: its directory, its account, its
    /// conversation.
    AttachJob(Box<giverny_claude::jobs::Job>),
    /// Group the rail by categories, or by repository.
    SetRailView(giverny_core::state::RailView),
    /// Fold a repository's group away. The empty path is the "no repo" group.
    ToggleRepoCollapse(PathBuf),
}

/// One Ctrl+Tab walk. The order is snapshotted at the first press so that
/// stepping through it cannot reshuffle the ground underneath — the walk
/// ends where Windows ends it, when Ctrl comes up.
struct Switcher {
    from: TabId,
    order: Vec<TabId>,
    index: usize,
}

/// A tab stopped by a usage limit, and what it would take to pick it up.
struct Limited {
    /// When the window reopens, once the numbers say. `None` until then: the
    /// account's cache refreshes on its own clock, so this is filled in later.
    reopens: Option<jiff::Timestamp>,
    /// The tab's input counter when it stopped. If it moves, someone has been
    /// here since, and this is theirs to continue rather than ours.
    untouched_at: u64,
    /// Whether Claude itself is still running: a session that exited has to be
    /// resumed first, one still sitting at its prompt only needs asking.
    session_gone: bool,
}

/// An install running in its tab, from the update button until the binary
/// lands or the installer stops without replacing it.
pub struct UpdateJob {
    pub tab: TabId,
    started: Instant,
    /// Something has run in the tab's shell. Until then an empty foreground
    /// is the command not having been typed yet, not the install having ended.
    seen_running: bool,
}

/// How long an install that was never seen running gets before it counts as
/// over: one that fails at once (no network) finishes between two looks.
const UPDATE_UNSEEN_GRACE: Duration = Duration::from_secs(10);

/// What one tab needs to open: the shell, the account it is on (named the
/// way Giverny stores accounts), and the environment that reaches the shell.
struct TabShape {
    shell: Option<(String, Vec<String>)>,
    /// What `SpawnCfg` should export as `CLAUDE_CONFIG_DIR`. `None` where the
    /// environment below says it better — or deliberately says nothing.
    config_dir: Option<PathBuf>,
    env: Vec<(String, String)>,
    /// The tab opens inside a distribution, where a Windows path means
    /// nothing to the shell and the shell's directory means nothing to
    /// Windows.
    in_wsl: bool,
}

pub struct TabRuntime {
    pub session: Option<TermSession>,
    pub view: TabView,
    /// Which prompt the bar over the terminal pins, kept between frames.
    pub prompt_bar: prompt_bar::Owner,
}

/// Rail width limits: narrow enough to be a strip, wide enough for long
/// tab titles, and the clamp a restored width is held to.
/// A thousand years in minutes, and more tokens than a context window will
/// ever hold: the two thresholds Claude Code compares a resumed session
/// against, set where nothing reaches them.
const FOREVER_MINUTES: u64 = 525_600_000;
const UNREACHABLE_TOKENS: u64 = 1_000_000_000;

const RAIL_MIN: f32 = 180.0;
const RAIL_MAX: f32 = 420.0;

/// How much scrollback a snapshot keeps, and how far behind the tab it is
/// allowed to fall.
///
/// Snapshots used to be written only on the way out, which left them worth
/// least in the case they exist for: a process that is killed rather than
/// closed never runs that code, so every tab came back showing whatever it
/// had at the last clean quit. Both kills on this machine were the kernel
/// picking a runaway process inside Giverny's cgroup, and neither left a
/// single snapshot behind.
///
/// One tab per tick rather than all of them at once: building a dump walks
/// the grid under the terminal lock, so a whole sweep on a timer would be a
/// stutter you could set your watch by.
const SNAPSHOT_ROWS: usize = 4000;
const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(60);

/// What was last written for a tab, so an unchanged screen costs no write.
struct Snapshot {
    at: Instant,
    hash: u64,
}

fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Shut down the way closing the window does when the close never comes
/// through the window: systemd stops the app's scope with SIGTERM at logout,
/// a launch from a terminal gets SIGINT from Ctrl-C, and a dying login
/// session sends SIGHUP. Nothing was listening for any of them, so those
/// exits skipped every write the shutdown path makes.
///
/// SIGKILL stays unstoppable — the periodic snapshot above is what bounds
/// its cost.
#[cfg(unix)]
fn shut_down_on_signal(ctx: egui::Context, asked: Arc<AtomicBool>) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(signals) => signals,
        Err(err) => {
            tracing::warn!("no signal handler installed: {err}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("giverny signals".into())
        // The iterator hands signals to an ordinary thread rather than to a
        // handler, so waking the UI from here is allowed.
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                tracing::info!("signal {signal}: saving and closing");
                asked.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("no signal thread: {err}");
    }
}

pub struct App {
    pub shared: RenderShared,
    pub ws: Workspace,
    pub rt: HashMap<TabId, TabRuntime>,
    /// In-progress inline rename: target + edit buffer.
    pub rename: Option<(RenameTarget, String)>,
    pub rename_needs_focus: bool,
    focus_terminal: bool,
    last_info_refresh: Instant,
    paths: Paths,
    state_dirty: bool,
    last_save: Instant,
    pub claude: claude_watch::ClaudeWatch,
    pub hooks_banner_dismissed: bool,
    /// Deferred automated actions (cwd fix, auto-resume) with due times.
    pending_inject: Vec<(Instant, TabId, Inject)>,
    pub palette: Option<overlays::PaletteState>,
    pub session_picker: Option<overlays::SessionPicker>,
    /// Last seen user-interaction counter per tab (detects "typed just now").
    input_seen: HashMap<TabId, u64>,
    /// Tab currently being dragged in the rail.
    pub dragging: Option<TabId>,
    /// Category currently being dragged in the rail. Never both at once: a
    /// drag starts on one thing.
    pub dragging_category: Option<CategoryId>,
    /// A Ctrl+Tab walk in progress: where it started, the recency order it
    /// snapshotted, and how far into it the user has stepped.
    switcher: Option<Switcher>,
    /// Wayland file drops, which winit 0.30 cannot deliver.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    dnd: Option<wayland_dnd::DragDrop>,
    /// Where a file drag currently hovers, in egui points. Set only on
    /// Wayland, where we track the drag ourselves and so know the position;
    /// winit's X11 drops report no coordinates at all.
    pub drag_hover: Option<egui::Pos2>,
    /// Tab rows as painted this frame, so a drag can be aimed at one.
    pub row_rects: Vec<(egui::Rect, TabId)>,
    /// Repository root per directory, so the sweep over every tab is one
    /// filesystem walk per distinct directory rather than per tab.
    repo_cache: HashMap<PathBuf, Option<PathBuf>>,
    #[allow(clippy::type_complexity)]
    repo_rx:
        Option<crossbeam_channel::Receiver<Vec<(TabId, PathBuf, Option<PathBuf>, Option<String>)>>>,
    /// Directories for tabs inside WSL, asked of the distribution itself
    /// because nothing on this side of the boundary knows them.
    wsl_cwd_rx: Option<crossbeam_channel::Receiver<Vec<(String, String, String)>>>,
    last_wsl_probe: Instant,
    /// How many tabs wanted you at the last frame, so the taskbar is only
    /// told when that changes.
    attention: usize,
    /// The window has no decorations and draws its own caption (#69).
    frameless: bool,
    /// The window was opened see-through and can show it, so
    /// `window.opacity` applies (see `translucency`).
    see_through: bool,
    /// How that window is maximised (#78).
    maximize: titlebar::Maximize,
    /// That window's cursors, sized to the display (#100).
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    cursors: Option<wslg_cursor::Cursors>,
    /// Tabs whose session stopped because the account ran out of limit.
    limited: HashMap<TabId, Limited>,
    /// Each tab's Claude state as of the last frame: stopping is a transition,
    /// not a state, and only the transition is worth looking at a screen for.
    was: HashMap<TabId, claude_watch::ClaudeState>,
    /// The welcome screen owed to this launch, until the tab for it opens.
    welcome: Option<splash::Welcome>,
    /// A newer release, once the background check finds one.
    pub update: Option<update::Available>,
    update_rx: Option<crossbeam_channel::Receiver<Option<update::Available>>>,
    pub update_dismissed: bool,
    /// The installer replaces the binary under the running process; these say
    /// when to offer the restart that picks it up.
    exe_mtime: Option<std::time::SystemTime>,
    pub update_job: Option<UpdateJob>,
    pub update_installed: bool,
    /// The last install ended without replacing the binary; its tab says why.
    pub update_stalled: bool,
    /// Theme-derived colours for Giverny's own chrome.
    pub chrome: chrome::Chrome,
    pub settings: Option<settings_ui::SettingsState>,
    pub keys_overlay: Option<keymap::KeysOverlay>,
    capture: Option<capture::Capture>,
    /// Last scrollback written per live tab.
    snapshots: HashMap<TabId, Snapshot>,
    /// Whether this process is on its way out on purpose, which is the
    /// difference between a clean shutdown and a crash in the state file.
    closing: bool,
    /// Set by the signal thread; read on the next frame.
    terminating: Arc<AtomicBool>,
    /// Window size and rail width as last seen, persisted with the workspace.
    layout: state::Layout,
    pub cfg: config::Config,
    cfg_mtime: Option<std::time::SystemTime>,
    /// When a clock-driven theme last looked at the clock.
    theme_tick: std::time::Instant,
    last_cfg_check: Instant,
    /// The config could not be parsed at startup, so the accounts were not
    /// set up from it; the first reload that parses does that.
    accounts_unset: bool,
}

/// Automated per-tab injections. All stand down once the user has typed.
#[derive(Debug, Clone)]
enum Inject {
    /// Raw bytes typed into the shell (the resume command).
    Raw(Vec<u8>),
    /// Verify the shell is in the expected directory; shell rc files that
    /// `cd` on startup (e.g. `cd ~/Dev`) override our spawn cwd — type a
    /// visible `cd` back when that happened.
    CwdFix(PathBuf),
    /// Typing into a tab the user has already worked in — waking a session a
    /// usage limit stopped. `Raw` stands down for any tab that has ever been
    /// typed in, which is every tab this applies to; the guard here is
    /// narrower and the right one: nobody has touched it *since it stopped*.
    Wake { bytes: Vec<u8>, seq: u64 },
}

/// Start our own Wayland drag-and-drop listener, on the connection eframe
/// already has. Returns `None` on X11, where winit delivers drops itself.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn start_wayland_dnd(cc: &eframe::CreationContext<'_>) -> Option<wayland_dnd::DragDrop> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::Wayland(surface) = cc.window_handle().ok()?.as_raw() else {
        return None;
    };
    let wake = cc.egui_ctx.clone();
    Some(wayland_dnd::DragDrop::start(
        surface.surface.as_ptr(),
        move || wake.request_repaint(),
    ))
}

/// Can account setup follow this config? Not when a value it reads — the
/// `[claude]` section — is one that did not fit and was stood in for.
fn accounts_readable(parsed: &config::Parsed) -> bool {
    !parsed.invalid_under("claude")
}

/// Bring every account in line with the config at startup: auto mode.
///
/// Not when the config could not be parsed, or its `[claude]` values could
/// not (`config_read` false). The app then runs on defaults there, and
/// following those would rewrite accounts against what the user configured.
/// Once the file parses again, the hot reload applies what it says. Hook
/// paths and the status line read nothing from the config, so
/// [`claude_watch::ClaudeWatch::new`] brings those up to date either way.
fn set_up_accounts(
    claude: &mut claude_watch::ClaudeWatch,
    cfg: &config::Config,
    config_read: bool,
) {
    if !config_read {
        tracing::warn!(
            "config.toml could not be parsed: Claude account settings left as they are until it is fixed"
        );
        return;
    }
    if cfg.claude.auto_mode {
        claude.ensure_auto_mode();
    }
}

/// Environment every tab's shell inherits, so `claude` behaves the way the
/// settings screen says however it is started — typed, resumed, or attached.
fn claude_env(claude: &config::ClaudeConfig) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if claude.skip_resume_summary {
        // Claude Code offers "resume from summary" for a session older than
        // 70 minutes and bigger than 100k tokens, and reads both thresholds
        // from the environment. Put them out of reach and the question never
        // comes up, which is the same answer as picking "Resume full session
        // as-is" every time.
        env.push((
            "CLAUDE_CODE_RESUME_THRESHOLD_MINUTES".into(),
            FOREVER_MINUTES.to_string(),
        ));
        env.push((
            "CLAUDE_CODE_RESUME_TOKEN_THRESHOLD".into(),
            UNREACHABLE_TOKENS.to_string(),
        ));
    }
    env
}

/// How to open a shell in one distribution.
///
/// `wsl.exe ~` is the documented shorthand for "start in the Linux home", and
/// it is *only* a shorthand: put `-d` in front of it and wsl.exe stops reading
/// the `~` as a place and starts reading it as the command to run, which bash
/// dutifully expands and tries to execute — `/home/ita: Is a directory`. So
/// the shorthand is used exactly where it works, which is also the common
/// case, and naming a distribution switches to `--cd`, the documented flag for
/// the same thing.
///
/// Arguments reach `CreateProcess` joined with spaces and no quoting added
/// (`escape_args: false`, so that a shell command can carry its own), which
/// makes quoting a distribution called "Ubuntu 22.04" this function's job.
fn wsl_shell(
    distro: &str,
    default: Option<&str>,
    start_dir: Option<&str>,
) -> (String, Vec<String>) {
    let mut args: Vec<String> = Vec::new();
    if default.is_none_or(|d| d != distro) {
        args.push("-d".into());
        args.push(wsl_arg(distro));
    }
    match start_dir {
        Some(dir) => {
            args.push("--cd".into());
            args.push(wsl_arg(dir));
        }
        // The shorthand, where it is the whole argument list and therefore
        // means what it says.
        None if args.is_empty() => args.push("~".into()),
        None => {
            args.push("--cd".into());
            args.push("~".into());
        }
    }
    ("wsl.exe".to_string(), args)
}

/// One argument for a command line that is joined without escaping.
fn wsl_arg(value: &str) -> String {
    if value.contains(' ') {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// The directory a tab inside a distribution reopens in.
///
/// A WSL shell reports its directory over OSC 7 as the unix path it is, which
/// Windows cannot open and every spawn until now therefore threw away — the
/// tab went home instead of back where it was. It can be checked over the
/// share, and it has to be: `--cd` on a directory that is gone is a tab that
/// does not open at all, rather than one that opens somewhere else.
fn wsl_start_dir(distro: &str, was_in: Option<&Path>) -> Option<String> {
    let text = was_in?.to_str()?;
    if !text.starts_with('/') {
        return None;
    }
    wsl::unc_path(distro, text)
        .is_dir()
        .then(|| text.to_string())
}

/// What restoring one conversation would do, or why it would not.
///
/// The app follows it and `giverny doctor` prints it, so what doctor says is
/// what the app does rather than a second opinion about it.
#[derive(Debug)]
enum ResumePlan {
    /// The recorded id is not a session id.
    Malformed,
    /// Something is already running this conversation. Two claudes resumed
    /// onto one transcript interleave it.
    LiveElsewhere,
    /// No account holds a transcript for it — `claude --resume` would fail.
    NoTranscript,
    Ready {
        account: PathBuf,
        /// Where the conversation ran, which is the only place it can be
        /// resumed from.
        dir: Option<PathBuf>,
        command: String,
    },
}

/// Plan the resume of `sid` for a tab last seen in `tab_cwd`, on `preferred`
/// account, given every account there is.
fn resume_plan(
    sid: &str,
    tab_cwd: Option<&Path>,
    preferred: Option<&Path>,
    all_dirs: &[PathBuf],
) -> ResumePlan {
    use giverny_claude::registry;
    if sid.len() != 36 || !sid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return ResumePlan::Malformed;
    }
    if registry::session_is_live(all_dirs.to_vec(), sid) {
        return ResumePlan::LiveElsewhere;
    }
    // The account the tab was on first, then the others: a transcript found
    // elsewhere self-heals a lost account association.
    let mut search: Vec<PathBuf> = preferred.map(Path::to_path_buf).into_iter().collect();
    search.extend(
        all_dirs
            .iter()
            .filter(|d| Some(d.as_path()) != preferred)
            .cloned(),
    );
    let Some((account, transcript)) = search
        .iter()
        .find_map(|d| registry::find_transcript(d, sid).map(|t| (d.clone(), t)))
    else {
        return ResumePlan::NoTranscript;
    };

    let dir = registry::transcript_cwd(&transcript).or_else(|| tab_cwd.map(Path::to_path_buf));
    let mut command = String::new();
    if let Some(dir) = &dir {
        command.push_str(&format!("cd \"{}\" && ", dir.display()));
    }
    // The command is typed into the tab's own shell, so the account has to be
    // named the way that shell can open it: a session inside WSL knows
    // `/home/x/.claude`, never the Windows share it is stored under here.
    // And a default account is not named at all — that is what put a first-run
    // prompt in front of every resumed conversation in a distribution.
    if giverny_claude::profiles::must_be_named(&account) {
        let named = match wsl::split_unc(&account) {
            Some((_, unix)) => unix,
            None => account.display().to_string(),
        };
        command.push_str(&format!("CLAUDE_CONFIG_DIR=\"{named}\" "));
    }
    // `command` bypasses shell wrapper functions named `claude`.
    command.push_str(&format!("command claude --resume {sid}\r"));
    ResumePlan::Ready {
        account,
        dir,
        command,
    }
}

/// `%WSLENV%` for a tab that opens in a distribution: the variables that have
/// to survive the crossing, added to whatever the user already shares.
///
/// Both directions matter. Going in, `GIVERNY_TAB_ID` is what makes a hook
/// belong to a tab; coming back out, the hook runs this binary as a Windows
/// process and needs the same variables to arrive with it.
///
/// Only variables actually being set may be listed. A name in `%WSLENV%` with
/// nothing behind it does not arrive absent — it arrives *empty*, which is a
/// different thing entirely to whoever reads it, and for `CLAUDE_CONFIG_DIR`
/// it is the difference between "use the default account" and "the account at
/// the empty path".
fn wslenv(inherited: Option<String>, ours: &[&str]) -> String {
    let mut parts: Vec<String> = inherited
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.split(':')
                // A trailing colon is common (Windows Terminal writes one)
                // and an empty entry is not a variable.
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for name in ours.iter().copied() {
        // An entry may carry a flag (`VAR/p`); the name before it is the key.
        if !parts
            .iter()
            .any(|p| p.split('/').next().is_some_and(|n| n == name))
        {
            parts.push(name.to_string());
        }
    }
    parts.join(":")
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        zoom: Option<f32>,
        frameless: bool,
        maximize: titlebar::Maximize,
        see_through: bool,
    ) -> Self {
        let paths = Paths::default_dirs();
        // A config that cannot be parsed runs on defaults, but those are not
        // what the user chose, so nothing is written into the accounts from
        // them (see `set_up_accounts`). Nor when only a value account setup
        // reads is unusable: one bad value is otherwise just left out.
        let (mut cfg, config_read) =
            match config::load_checked(paths.base(), &config::Config::default()) {
                Ok(parsed) => {
                    let read = accounts_readable(&parsed);
                    (parsed.config, read)
                }
                Err(err) => {
                    tracing::error!("config.toml ignored ({err}); running on defaults");
                    (config::Config::default(), false)
                }
            };
        remember_env_accounts(&paths, &mut cfg);
        let theme = theme_for(&cfg.theme.name);
        let family = (!cfg.font.family.is_empty()).then_some(cfg.font.family.as_str());
        let mut shared = RenderShared::with_family(theme, cfg.font.size, family)
            .or_else(|err| {
                tracing::warn!("configured font unusable ({err}); auto-detecting");
                RenderShared::new(theme_for(&cfg.theme.name), cfg.font.size)
            })
            .expect("font discovery");
        shared.install_ui_fonts(&cc.egui_ctx);
        giverny_term::pace::set_cheap_frames(draws_on_gpu(cc));
        let see_through = see_through && translucency::confirm(cc);
        shared.opacity = opacity_for(see_through, &cfg);
        let chrome = chrome::Chrome::from_theme(&theme_for(&cfg.theme.name));
        chrome.apply(&cc.egui_ctx, &theme_for(&cfg.theme.name), shared.opacity);

        let mut cfg_mtime = config_mtime(&paths);
        let restored = state::load(&paths);
        // Say so when the last run ended without going through the shutdown
        // path — a kill, an OOM, or a compositor that took the session with
        // it. The marker was written and never read, which made every crash
        // look from the logs like an ordinary quit.
        if restored.as_ref().is_some_and(|st| !st.clean_shutdown) {
            tracing::warn!(
                "last run did not shut down cleanly; tabs restore from their last snapshot"
            );
        }
        // Font size now lives in config.toml alone. One-time migration for
        // state files written when it lived here instead, so nobody's zoom
        // level is lost the first time they run this build.
        if let Some(st) = &restored
            && st.font_size != DEFAULT_FONT_SIZE
            && cfg.font.size == DEFAULT_FONT_SIZE
        {
            shared.set_font_size(st.font_size);
            if let Some(def) = giverny_core::settings::by_key("font.size") {
                let value = giverny_core::settings::Value::Float(st.font_size as f64);
                if giverny_core::settings::write(paths.base(), def, &value).is_ok() {
                    cfg.font.size = st.font_size;
                    cfg_mtime = config_mtime(&paths);
                }
            }
        }
        let layout = restored
            .as_ref()
            .map(|st| st.layout.clone())
            .unwrap_or_default();
        let mut ws = restored
            .map(|st| st.workspace)
            .filter(|ws| !ws.tabs.is_empty())
            .unwrap_or_default();

        // Update check runs on its own thread so a slow network never
        // delays startup; the UI picks the answer up when it lands.
        let update_rx = if cfg.update.check {
            let (tx, rx) = crossbeam_channel::unbounded();
            let base = paths.base().to_path_buf();
            let ping = cc.egui_ctx.clone();
            std::thread::Builder::new()
                .name("giverny update check".into())
                .spawn(move || {
                    // Keep asking. A window left open for a fortnight used to
                    // check once, at startup, and never again.
                    loop {
                        let found = update::check(&base, true);
                        if tx.send(found).is_err() {
                            return;
                        }
                        ping.request_repaint();
                        std::thread::sleep(Duration::from_secs(update::CHECK_INTERVAL_SECS));
                    }
                })
                .ok()
                .map(|_| rx)
        } else {
            None
        };

        let wake_ctx = cc.egui_ctx.clone();
        let (claude, spooled) = claude_watch::ClaudeWatch::new(
            &paths.hook_spool(),
            &cfg.behavior.extra_profile_dirs,
            move || wake_ctx.request_repaint(),
        );
        // Events spooled while the app was closed: keep session captures.
        for msg in &spooled {
            let Some(id) = claude_watch::ClaudeWatch::tab_id_of(msg) else {
                continue;
            };
            // SessionStart only, deliberately. These are the messages that
            // arrived while the app was closed — and what ends every session
            // in a tab is the app closing. Claude Code runs its SessionEnd
            // hook on the way down, the relay spools it because there is
            // nothing listening any more, and replaying that here erased the
            // conversation of every tab that had one, seconds before the tab
            // was restored and asked what to resume. A live SessionEnd still
            // clears it: that one means the user left.
            if let Some("SessionStart") = msg.hook_event()
                && let Some(tab) = ws.tab_mut(id)
            {
                tab.claude_session = msg.session_id().map(str::to_string);
                tab.claude_config_dir = msg.config_dir.as_deref().map(std::path::PathBuf::from);
            }
        }

        // Asked before the workspace moves into the app: a restored tab is how
        // an upgrade is told apart from a first run.
        let welcome = splash::due(paths.base(), !ws.tabs.is_empty());
        let mut app = App {
            shared,
            ws,
            rt: HashMap::new(),
            rename: None,
            rename_needs_focus: false,
            focus_terminal: true,
            last_info_refresh: Instant::now(),
            paths,
            state_dirty: false,
            last_save: Instant::now(),
            claude,
            hooks_banner_dismissed: false,
            pending_inject: Vec::new(),
            palette: None,
            session_picker: None,
            input_seen: HashMap::new(),
            dragging: None,
            dragging_category: None,
            switcher: None,
            #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
            dnd: start_wayland_dnd(cc),
            drag_hover: None,
            row_rects: Vec::new(),
            attention: 0,
            frameless,
            see_through,
            maximize,
            #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
            cursors: frameless
                .then(|| wslg_cursor::Cursors::new(cc, wslg::desktop_scale()))
                .flatten(),
            limited: HashMap::new(),
            was: HashMap::new(),
            repo_cache: HashMap::new(),
            repo_rx: None,
            wsl_cwd_rx: None,
            last_wsl_probe: Instant::now() - Duration::from_secs(60),
            update: None,
            update_rx,
            welcome,
            update_dismissed: false,
            exe_mtime: update::binary_mtime(),
            update_job: None,
            update_installed: false,
            update_stalled: false,
            chrome,
            settings: None,
            keys_overlay: None,
            capture: capture::Capture::from_env(),
            snapshots: HashMap::new(),
            closing: false,
            terminating: Arc::new(AtomicBool::new(false)),
            layout,
            cfg_mtime,
            theme_tick: std::time::Instant::now(),
            cfg,
            last_cfg_check: Instant::now(),
            accounts_unset: !config_read,
        };
        if let Some(z) = zoom {
            cc.egui_ctx.set_zoom_factor(z);
            app.layout.zoom = Some(z);
        }
        #[cfg(unix)]
        shut_down_on_signal(cc.egui_ctx.clone(), app.terminating.clone());
        set_up_accounts(&mut app.claude, &app.cfg, config_read);
        if app.ws.tabs.is_empty() {
            let cat = app.ws.categories[0].id;
            app.apply(
                &cc.egui_ctx,
                Action::NewTab {
                    category: cat,
                    cwd: None,
                },
            );
        }
        // Sessions for restored tabs spawn lazily on first focus; the state
        // file is rewritten now, marked as a crash until a shutdown says
        // otherwise.
        app.save_state();
        app
    }

    /// A brand-new tab. Tab ids start over when the state file is lost, so
    /// a history file can outlive its tab (one whose shell outlasted the
    /// close, say): a new tab with that id starts without it.
    fn new_tab(&mut self, category: CategoryId) -> TabId {
        let id = self.ws.add_tab(category);
        history::remove(&self.paths, id);
        id
    }

    /// Write one tab's scrollback, skipping the write when the screen has not
    /// changed since the last one — an idle tab would otherwise cost an fsync
    /// a minute for a file already holding exactly those bytes.
    fn snapshot_tab(&mut self, id: TabId) {
        let dump = self
            .rt
            .get(&id)
            .and_then(|rt| rt.session.as_ref())
            .and_then(|session| session.snapshot_ansi(SNAPSHOT_ROWS));
        // `None` means the alt screen is up: vim or a full-screen Claude owns
        // the terminal, and what it draws is not what the tab should come back
        // as. Keep whatever was saved before it took over.
        let Some(dump) = dump else { return };
        let hash = hash_of(&dump);
        let at = Instant::now();
        if self.snapshots.get(&id).is_some_and(|s| s.hash == hash) {
            self.snapshots.insert(id, Snapshot { at, hash });
            return;
        }
        match state::save_snapshot(&self.paths, id, &dump) {
            Ok(()) => {
                self.snapshots.insert(id, Snapshot { at, hash });
            }
            Err(err) => tracing::error!("snapshot save failed for {id:?}: {err:#}"),
        }
    }

    /// Snapshot the live tab that has gone longest without one.
    fn snapshot_stalest_tab(&mut self) {
        let due = self
            .rt
            .iter()
            .filter(|(_, rt)| rt.session.is_some())
            .map(|(&id, _)| id)
            .filter(|id| {
                self.snapshots
                    .get(id)
                    .is_none_or(|s| s.at.elapsed() >= SNAPSHOT_MAX_AGE)
            })
            // A tab that has never been snapshotted sorts first: `None` is
            // less than any `Some`.
            .min_by_key(|id| self.snapshots.get(id).map(|s| s.at));
        if let Some(id) = due {
            self.snapshot_tab(id);
        }
    }

    /// Everything that has to reach disk before the process ends. Safe to
    /// call twice — the second pass finds nothing changed.
    fn persist_all(&mut self) {
        let ids: Vec<TabId> = self.rt.keys().copied().collect();
        for id in ids {
            self.snapshot_tab(id);
        }
        self.save_state();
    }

    /// The marker is taken from `closing` rather than passed in: the frames
    /// between asking to close and the process actually ending still save,
    /// and any one of them saying "clean: no" would undo the record of a
    /// deliberate quit.
    fn save_state(&mut self) {
        let st = SaveState {
            version: state::STATE_VERSION,
            boot_id: state::boot_id(),
            clean_shutdown: self.closing,
            workspace: self.ws.clone(),
            font_size: self.shared.font_size,
            layout: self.layout.clone(),
        };
        if let Err(err) = state::save(&self.paths, &st) {
            tracing::error!("state save failed: {err:#}");
        }
        self.state_dirty = false;
        self.last_save = Instant::now();
    }

    pub fn apply(&mut self, ctx: &egui::Context, action: Action) {
        self.state_dirty = true;
        match action {
            Action::NewTab { category, cwd } => {
                let cwd = cwd
                    .or_else(|| self.ws.active_tab().and_then(|t| t.cwd.clone()))
                    .or_else(dirs::home_dir)
                    .unwrap_or_else(|| PathBuf::from("/"));
                let id = self.new_tab(category);
                self.ws.tab_mut(id).unwrap().cwd = Some(cwd);
                self.spawn_session(ctx, id, None);
                self.reveal_terminal();
            }
            Action::NewCategory => {
                let n = self.ws.categories.len() + 1;
                let id = self.ws.add_category(&format!("category {n}"));
                self.rename = Some((RenameTarget::Category(id), format!("category {n}")));
                self.rename_needs_focus = true;
            }
            Action::CloseTab(id) => {
                if let Some(rt) = self.rt.remove(&id)
                    && let Some(session) = rt.session
                {
                    // Join off the UI thread; the loop exits quickly. The
                    // history goes once the shell has: zsh writes its file
                    // on the SIGHUP that shutdown sends, so removing it
                    // first would only see it written again.
                    let paths = self.paths.clone();
                    std::thread::spawn(move || {
                        // Longer than the 500 ms an app exit allows: this
                        // thread holds nothing up, and a shell that is slow
                        // to go would write its file after the removal.
                        session.shutdown_within(Duration::from_secs(10));
                        history::remove(&paths, id);
                    });
                } else {
                    history::remove(&self.paths, id);
                }
                self.ws.close_tab(id);
                state::remove_snapshot(&self.paths, id);
                self.snapshots.remove(&id);
                self.focus_terminal = true;
            }
            Action::Select(id) => {
                self.end_switch();
                self.ws.set_active(id);
                self.refresh_tab_info(id);
                self.claude.mark_viewed(id);
                self.reveal_terminal();
            }
            Action::SwitchRecent(delta) => self.switch_recent(delta),
            Action::Cycle(delta) => {
                self.end_switch();
                self.ws.cycle_active(delta);
                if let Some(id) = self.ws.active {
                    self.refresh_tab_info(id);
                    self.claude.mark_viewed(id);
                }
                self.reveal_terminal();
            }
            Action::SetRailView(view) => {
                self.layout.rail_view = view;
                self.state_dirty = true;
            }
            Action::ToggleRepoCollapse(repo) => {
                let folded = &mut self.layout.collapsed_repos;
                match folded.iter().position(|p| *p == repo) {
                    Some(at) => {
                        folded.remove(at);
                    }
                    None => folded.push(repo),
                }
                self.state_dirty = true;
            }
            Action::ToggleCollapse(id) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.collapsed = !cat.collapsed;
                }
            }
            Action::StartRename(target) => {
                let current = match &target {
                    RenameTarget::Tab(id) => self
                        .ws
                        .tab(*id)
                        // What they see, not the raw title behind it.
                        .map(|t| t.display_title(&self.cfg.titles))
                        .unwrap_or_default(),
                    RenameTarget::Category(id) => self
                        .ws
                        .category(*id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                };
                self.rename = Some((target, current));
                self.rename_needs_focus = true;
            }
            Action::CommitRename(target, value) => {
                self.rename = None;
                let Some(value) = value else { return };
                let value = value.trim().to_string();
                match target {
                    RenameTarget::Tab(id) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.custom_title = (!value.is_empty()).then_some(value);
                        }
                    }
                    RenameTarget::Category(id) => {
                        if !value.is_empty()
                            && let Some(cat) = self.ws.category_mut(id)
                        {
                            cat.name = value;
                        }
                    }
                }
            }
            Action::Respawn(id) => {
                self.spawn_session(ctx, id, None);
                self.queue_resume(id);
                self.focus_terminal = true;
            }
            Action::InstallHooks => match self.claude.install_hooks() {
                Ok(n) => tracing::info!("hooks installed into {n} profile(s)"),
                Err(e) => tracing::error!("hook install: {e}"),
            },
            Action::DismissHooksBanner => self.hooks_banner_dismissed = true,
            Action::SetCategoryProfile(id, dir) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.profile_dir = dir;
                }
            }
            Action::JumpAttention => {
                use claude_watch::ClaudeState;
                let order: Vec<TabId> = self.ws.tabs.iter().map(|t| t.id).collect();
                if order.is_empty() {
                    return;
                }
                let start = self
                    .ws
                    .active
                    .and_then(|a| order.iter().position(|&x| x == a))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let ring = |k: usize| order[(start + k) % order.len()];
                let pick = (0..order.len())
                    .map(ring)
                    .find(|&t| self.claude.state_of(t) == ClaudeState::NeedsYou)
                    .or_else(|| {
                        (0..order.len())
                            .map(ring)
                            .find(|&t| self.claude.state_of(t) == ClaudeState::DoneUnseen)
                    });
                if let Some(id) = pick {
                    self.apply(ctx, Action::Select(id));
                }
            }
            Action::ToggleStatusline(enable) => match self.claude.set_statusline(enable) {
                Ok(()) => tracing::info!(
                    "statusline {}",
                    if enable { "installed" } else { "removed" }
                ),
                Err(e) => tracing::error!("statusline: {e}"),
            },
            Action::RefreshUsage => self.claude.refresh_stale_usage(0, true),
            Action::ToggleSettings => {
                self.settings = match self.settings.take() {
                    Some(_) => None,
                    // Back on the section last looked at, not the first.
                    None => Some(settings_ui::SettingsState {
                        section: giverny_core::settings::Section::ALL
                            .iter()
                            .copied()
                            .find(|s| self.layout.settings_section.as_deref() == Some(s.title()))
                            .unwrap_or(giverny_core::settings::Section::Appearance),
                        ..Default::default()
                    }),
                };
                self.focus_terminal = self.settings.is_none();
            }
            Action::ToggleKeys => {
                self.keys_overlay = match self.keys_overlay.take() {
                    Some(_) => None,
                    None => Some(keymap::KeysOverlay::default()),
                };
            }
            Action::SetSetting(key, value) => {
                let Some(def) = giverny_core::settings::by_key(&key) else {
                    tracing::warn!("unknown setting {key}");
                    return;
                };
                match giverny_core::settings::write(self.paths.base(), def, &value) {
                    Ok(()) => {
                        // Apply now rather than waiting for the mtime poll, and
                        // record the mtime we just caused so the watcher does
                        // not reload the same content a second later.
                        self.reload_config(ctx);
                        self.cfg_mtime = config_mtime(&self.paths);
                    }
                    Err(err) => tracing::error!("could not write {key}: {err:#}"),
                }
            }
            Action::AttachJob(job) => {
                let Some(sid) = job.resume_target().map(str::to_string) else {
                    tracing::warn!("job {} has no conversation to attach to", job.id);
                    return;
                };
                let cat = category_for_agent(&mut self.ws, job.cwd.as_deref());
                let id = self.new_tab(cat);
                if let Some(tab) = self.ws.tab_mut(id) {
                    // The agent's own directory: `claude --resume` only finds a
                    // conversation from where it ran.
                    tab.cwd = job.cwd.clone().or_else(dirs::home_dir);
                    tab.custom_title = Some(job.name.clone());
                }
                // A conversation that is not on disk cannot be resumed, and a
                // shell opening on an empty screen looks like a tab that did
                // nothing. Say what happened, in the agent's own directory.
                if giverny_claude::registry::find_transcript(&job.config_dir, &sid).is_none() {
                    tracing::info!("job {}: no transcript for {sid}", job.id);
                    self.spawn_session(
                        ctx,
                        id,
                        Some(format!(
                            "\x1b[2mgiverny:\x1b[0m no conversation on disk for this agent\r\n                             \x1b[2m         {sid}\r\n                                      this tab is its directory; the agent itself is still                              wherever it is running.\x1b[0m\r\n\r\n"
                        )),
                    );
                    return;
                }
                self.spawn_session(ctx, id, None);
                self.apply(ctx, Action::ResumeSpecific(id, sid, job.config_dir.clone()));
            }
            Action::EditConfig => {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "nano".into());
                let path = config::config_path(self.paths.base());
                let cat = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id));
                if let Some(cat) = cat {
                    let id = self.new_tab(cat);
                    self.ws.tab_mut(id).unwrap().cwd = dirs::home_dir();
                    self.spawn_session(ctx, id, None);
                    // Same deferred injection the resume path uses: give the
                    // shell time to be ready before typing into it.
                    self.pending_inject.push((
                        Instant::now() + Duration::from_millis(700),
                        id,
                        Inject::Raw(format!("{editor} {}\n", path.display()).into_bytes()),
                    ));
                    self.settings = None;
                    self.focus_terminal = true;
                }
            }
            Action::TogglePalette => {
                self.palette = if self.palette.is_some() {
                    None
                } else {
                    Some(Default::default())
                };
            }
            Action::OpenSessions(id) => {
                let cwd = self
                    .rt
                    .get(&id)
                    .and_then(|rt| rt.session.as_ref())
                    .and_then(|s| s.proc_cwd())
                    .or_else(|| self.ws.tab(id).and_then(|t| t.cwd.clone()));
                let Some(cwd) = cwd else { return };
                let dirs: Vec<PathBuf> = self
                    .claude
                    .profiles
                    .iter()
                    .map(|p| p.config_dir.clone())
                    .collect();
                let sessions = giverny_claude::registry::list_sessions(&dirs, &cwd);
                self.session_picker = Some(overlays::SessionPicker { tab: id, sessions });
            }
            Action::ResumeSpecific(id, sid, config_dir) => {
                if let Some(tab) = self.ws.tab_mut(id) {
                    tab.claude_session = Some(sid.clone());
                    tab.claude_config_dir = Some(config_dir);
                }
                let has_live_shell = self.rt.get(&id).is_some_and(|rt| rt.session.is_some())
                    && !self.ws.tab(id).is_some_and(|t| t.exited);
                if has_live_shell {
                    if let Some(cmd) = self.resume_command(&sid, id) {
                        // Ctrl+U clears any half-typed line first.
                        let mut bytes = vec![0x15];
                        bytes.extend(cmd);
                        if let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) {
                            session.write(bytes);
                        }
                    }
                } else {
                    // Respawn queues the auto-resume for the new shell.
                    self.apply(ctx, Action::Respawn(id));
                }
                self.apply(ctx, Action::Select(id));
            }
            Action::DeleteCategory(id) => {
                self.ws.remove_category(id);
            }
            Action::SetCategoryColor(id, color_index) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.color_index = color_index;
                }
            }
            Action::MoveTab(tab, category) => {
                self.ws.move_tab_to_category(tab, category);
            }
            Action::ReorderCategory(id, index) => {
                self.ws.reorder_category(id, index);
                self.state_dirty = true;
            }
            Action::ReorderTab(tab, category, index) => {
                self.ws.reorder_tab(tab, category, index);
            }
            Action::RunUpdate => {
                // Deliberately visible: a new tab runs the same command a new
                // user would, so nothing rewrites the binary out of sight.
                let category = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id));
                if let Some(category) = category {
                    self.apply(
                        ctx,
                        Action::NewTab {
                            category,
                            cwd: None,
                        },
                    );
                    if let Some(id) = self.ws.active {
                        let mut cmd = update::install_command().into_bytes();
                        cmd.push(b'\r');
                        self.pending_inject.push((
                            Instant::now() + Duration::from_millis(1100),
                            id,
                            Inject::Raw(cmd),
                        ));
                        self.update_job = Some(UpdateJob {
                            tab: id,
                            started: Instant::now(),
                            seen_running: false,
                        });
                        self.update_stalled = false;
                    }
                }
            }
            Action::DismissUpdate => self.update_dismissed = true,
            Action::RestartNow => {
                // Everything worth keeping is on disk already: the workspace
                // saves itself as it goes, and a restored tab resumes its
                // conversation. Hand over to the new binary and go.
                self.closing = true;
                self.persist_all();
                let exe = std::env::current_exe().unwrap_or_else(|_| "giverny".into());
                match std::process::Command::new(exe)
                    .args(std::env::args_os().skip(1))
                    .spawn()
                {
                    Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(err) => tracing::warn!("cannot start the new version: {err}"),
                }
            }
        }
    }

    /// A tab with the welcome already on its screen.
    ///
    /// Seeded rather than typed: the text is in the terminal before the shell
    /// writes its first prompt, so nothing lands in anyone's history and the
    /// links arrive as hyperlinks rather than as something to copy out.
    fn open_welcome(&mut self, ctx: &egui::Context, welcome: &splash::Welcome) {
        let category = self
            .ws
            .active_tab()
            .map(|t| t.category)
            .or_else(|| self.ws.categories.first().map(|c| c.id));
        let Some(category) = category else { return };
        splash::mark_seen(self.paths.base());
        let id = self.new_tab(category);
        if let Some(tab) = self.ws.tab_mut(id) {
            tab.cwd = dirs::home_dir();
        }
        self.spawn_session(ctx, id, Some(splash::render(welcome)));
        self.reveal_terminal();
    }

    /// How this tab's shell is started so it keeps its own history:
    /// arguments for it and environment for Giverny's init file there. `None` when the setting is off, or the shell is not one that
    /// can be steered this way or already has arguments of its own. `env` is
    /// what the tab already adds on top of ours.
    fn history_steer(
        &self,
        id: TabId,
        shell: Option<&(String, Vec<String>)>,
        env: &[(String, String)],
    ) -> Option<(String, history::Steer)> {
        if !self.cfg.behavior.history_per_tab {
            return None;
        }
        if shell.is_some_and(|(_, args)| !args.is_empty()) {
            return None;
        }
        let program = pty::shell_program(shell)?;
        let kind = history::ShellKind::of(&program)?;
        let file = history::history_file(&self.paths, id);
        let init = history::init_dir(&self.paths);
        // bash and zsh create the file, not the directory it sits in.
        let ready = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| history::write_init(&init));
        if let Err(err) = ready {
            tracing::warn!("no per-tab history for tab {}: {err}", id.0);
            return None;
        }
        let home = dirs::home_dir();
        let steer = history::steer(
            kind,
            &file,
            id,
            self.cfg.behavior.history_also_shared,
            home.as_deref(),
            &init,
            |var| {
                env.iter()
                    .rev()
                    .find(|(k, _)| k == var)
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var(var).ok())
            },
        );
        Some((program, steer))
    }

    fn spawn_session(&mut self, ctx: &egui::Context, id: TabId, preseed: Option<String>) {
        let Some(tab) = self.ws.tab(id) else { return };
        let cwd = tab
            .cwd
            .clone()
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let profile_dir = self
            .ws
            .tab(id)
            .and_then(|t| self.ws.category(t.category))
            .and_then(|c| c.profile_dir.clone());
        let was_in = self.ws.tab(id).and_then(|t| t.cwd.clone());
        let mut shape = self.tab_shape(profile_dir, was_in.as_deref());
        let in_wsl = shape.in_wsl;
        // A WSL shell is a world of its own — its paths, its home — and is
        // left with the history it has.
        if !in_wsl
            && let Some((program, steer)) = self.history_steer(id, shape.shell.as_ref(), &shape.env)
        {
            if !steer.args.is_empty() {
                shape.shell = Some((program, steer.args));
            }
            shape.env.extend(steer.env);
        }
        let cfg = SpawnCfg {
            shell: shape.shell,
            cwd: cwd.clone(),
            env_extra: shape.env,
            tab_id: format!("giverny-{}", id.0),
            nonce: fresh_nonce(id.0),
            claude_config_dir: shape.config_dir,
            size: GridSize {
                cols: 120,
                rows: 30,
                cell_width: 9,
                cell_height: 18,
            },
        };
        match TermSession::spawn(
            &cfg,
            ctx.clone(),
            self.shared.theme.clone(),
            preseed.as_deref(),
        ) {
            Ok(session) => {
                self.ws.tab_mut(id).unwrap().exited = false;
                let entry = self.rt.entry(id).or_insert_with(|| TabRuntime {
                    session: None,
                    view: TabView::default(),
                    prompt_bar: prompt_bar::Owner::default(),
                });
                entry.session = Some(session);
                // A new session counts its changes from nothing again.
                entry.prompt_bar = prompt_bar::Owner::default();
                // Startup rc files may `cd` away from the spawn dir; verify
                // and correct once the shell has settled. Not across the WSL
                // boundary: what Windows can see of `wsl.exe` is its own
                // working directory, which says nothing about where the shell
                // inside it is, and typing a Windows path at a bash prompt
                // would be the only outcome of comparing them.
                if !in_wsl {
                    self.pending_inject.push((
                        Instant::now() + Duration::from_millis(900),
                        id,
                        Inject::CwdFix(cwd),
                    ));
                }
            }
            Err(err) => {
                tracing::error!("spawn failed for tab {id:?}: {err:#}");
                if let Some(tab) = self.ws.tab_mut(id) {
                    tab.exited = true;
                    tab.auto_title = format!("spawn failed: {err}");
                }
            }
        }
    }

    fn drain_events(&mut self) {
        let mut cwd_updates: Vec<(TabId, PathBuf)> = Vec::new();
        for (&id, rt) in &self.rt {
            let Some(session) = &rt.session else { continue };
            while let Ok(ev) = session.events.try_recv() {
                match ev {
                    TabEvent::Title(title) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.auto_title = title.unwrap_or_default();
                        }
                    }
                    TabEvent::Tee(events) => {
                        for te in events {
                            match te {
                                TeeEvent::CwdChanged(p) => cwd_updates.push((id, p)),
                                TeeEvent::RemoteCwd(p) => {
                                    tracing::debug!("remote cwd on tab {id:?}: {p:?}");
                                }
                                other => tracing::debug!("tee {id:?}: {other:?}"),
                            }
                        }
                    }
                    TabEvent::Bell => tracing::debug!("bell on tab {id:?}"),
                    TabEvent::ChildExit(status) => {
                        tracing::info!("tab {id:?} child exited: {status:?}");
                    }
                    TabEvent::LoopDone(_) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.exited = true;
                        }
                    }
                }
            }
        }
        for (id, cwd) in cwd_updates {
            if let Some(tab) = self.ws.tab_mut(id) {
                tab.cwd = Some(cwd);
                self.state_dirty = true;
            }
            if let Some(local) = self.ws.tab(id).and_then(|t| self.local_path(t)) {
                let branch = giverny_core::git::branch_of(&local);
                if let Some(tab) = self.ws.tab_mut(id) {
                    tab.git_branch = branch;
                }
            }
        }
    }

    /// Notice a session that stopped because the account ran out of limit,
    /// and pick it up when the window reopens.
    ///
    /// Nothing reports this: the `Stop` hook is the same one a finished turn
    /// sends, and the usage cache is a percentage with no opinion about why a
    /// session ended. What says it is the message left on the tab's screen, so
    /// that is what gets read — but only at the moment a tab stops working,
    /// and only for that tab, because reading a screen means locking a
    /// terminal.
    fn watch_for_limits(&mut self) {
        use claude_watch::ClaudeState;
        let now = jiff::Timestamp::now();
        let states: Vec<(TabId, ClaudeState)> = self
            .ws
            .tabs
            .iter()
            .map(|t| (t.id, self.claude.state_of(t.id)))
            .collect();

        for (id, state) in states {
            let before = self.was.insert(id, state);
            // Someone came back to it: theirs now.
            if let Some(waiting) = self.limited.get(&id) {
                let touched = self
                    .rt
                    .get(&id)
                    .and_then(|rt| rt.session.as_ref())
                    .is_some_and(|s| s.input_seq() != waiting.untouched_at);
                if touched || state == ClaudeState::Busy {
                    self.limited.remove(&id);
                }
            }
            // Stopping is the moment to look, and the only one.
            let stopped = before == Some(ClaudeState::Busy) && state != ClaudeState::Busy;
            if !stopped || self.limited.contains_key(&id) {
                continue;
            }
            let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) else {
                continue;
            };
            if !giverny_claude::usage::looks_rate_limited(&session.screen_text()) {
                continue;
            }
            let account = self.claude.tabs.get(&id).and_then(|t| t.account.clone());
            let reopens = self.claude.window_reopens(account.as_deref());
            tracing::info!(
                "tab {id:?}: out of limit{}",
                match reopens {
                    Some(at) => format!("; window reopens {at}"),
                    None => String::new(),
                }
            );
            self.limited.insert(
                id,
                Limited {
                    reopens,
                    untouched_at: session.input_seq(),
                    session_gone: state == ClaudeState::None,
                },
            );
        }

        if self.limited.is_empty() {
            return;
        }
        // Fill in reset times that were not known when the tab stopped, and
        // wake the ones whose window has come round.
        let ready: Vec<TabId> = self
            .limited
            .iter_mut()
            .filter_map(|(id, waiting)| {
                if waiting.reopens.is_none() {
                    let account = self.claude.tabs.get(id).and_then(|t| t.account.clone());
                    waiting.reopens = self.claude.window_reopens(account.as_deref());
                }
                waiting.reopens.filter(|at| *at <= now).map(|_| *id)
            })
            .collect();
        for id in ready {
            let Some(waiting) = self.limited.remove(&id) else {
                continue;
            };
            if !self.cfg.claude.resume_after_limit {
                tracing::info!("tab {id:?}: window reopened (resume_after_limit is off)");
                continue;
            }
            self.carry_on(id, waiting.session_gone);
        }
    }

    /// Ask a tab to pick up where the limit stopped it.
    ///
    /// A session that is still running only needs the ask. One that exited has
    /// to be brought back first, and the prompt follows far enough behind for
    /// Claude Code to be listening — the same settle delay a restored tab uses,
    /// and the same standing-down if the user starts typing in the meantime.
    fn carry_on(&mut self, id: TabId, session_gone: bool) {
        const CONTINUE: &str = "continue\r";
        let seq = self
            .rt
            .get(&id)
            .and_then(|rt| rt.session.as_ref())
            .map(|s| s.input_seq())
            .unwrap_or_default();
        let wake = |bytes: &str| Inject::Wake {
            bytes: bytes.as_bytes().to_vec(),
            seq,
        };
        if !session_gone {
            tracing::info!("tab {id:?}: window reopened, asking it to continue");
            self.pending_inject
                .push((Instant::now(), id, wake(CONTINUE)));
            return;
        }
        let Some(sid) = self.session_to_resume(id) else {
            tracing::info!("tab {id:?}: window reopened, but no conversation to resume");
            return;
        };
        let Some(cmd) = self.resume_command(&sid, id) else {
            return;
        };
        tracing::info!("tab {id:?}: window reopened, resuming {sid} to continue");
        self.pending_inject.push((
            Instant::now() + Duration::from_millis(1300),
            id,
            Inject::Wake { bytes: cmd, seq },
        ));
        self.pending_inject
            .push((Instant::now() + Duration::from_secs(9), id, wake(CONTINUE)));
    }

    /// Say outside the window what the rail says inside it.
    ///
    /// A tab that wants you is invisible from another application, which is
    /// where you are when a tab starts wanting you. The window title carries
    /// the count — it is what a hover over the taskbar button shows, and what
    /// Alt-Tab shows — and the first tab to want anything asks the desktop for
    /// attention, which is a highlighted taskbar button on Windows, a bouncing
    /// dock icon on macOS and an urgency hint on Linux. Only ever on a change:
    /// a title set every frame is a title flickering every frame.
    fn show_attention(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        use claude_watch::ClaudeState;
        let waiting = self
            .claude
            .tabs
            .values()
            .filter(|t| t.state == ClaudeState::NeedsYou)
            .count();
        if waiting == self.attention {
            return;
        }
        let first = waiting > self.attention;
        self.attention = waiting;
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(if waiting > 0 {
            format!("Giverny ({waiting})")
        } else {
            "Giverny".to_string()
        }));
        // Windows can wear the mark on the button itself, which outlasts the
        // highlight below: it stays until it is taken away.
        if let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(frame)
            && let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            taskbar::set(win32.hwnd.get(), waiting);
        }
        // Asking for attention while the window already has it is how a
        // window ends up flashing at someone looking straight at it.
        if first && !ctx.input(|i| i.focused) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }

    /// Ask each distribution where its tabs are, and apply the last answer.
    ///
    /// The equivalent of the `/proc` read below, for tabs whose `/proc` is on
    /// the other side of the boundary. It costs a `wsl.exe` launch, so it runs
    /// on its own slower clock and off the UI thread; the tab that moved a
    /// second ago is not urgent, the tab that reopens tomorrow in the right
    /// place is the point.
    fn probe_wsl_cwds(&mut self) {
        // One take, three outcomes: an answer, a thread that died without
        // one (which must not wedge every later sweep by leaving the
        // receiver in place), or nothing yet.
        let answer = self.wsl_cwd_rx.as_ref().map(|rx| rx.try_recv());
        match answer {
            Some(Err(crossbeam_channel::TryRecvError::Empty)) | None => {}
            Some(Err(crossbeam_channel::TryRecvError::Disconnected)) => self.wsl_cwd_rx = None,
            Some(Ok(found)) => {
                self.wsl_cwd_rx = None;
                for (tab, distro, cwd) in found {
                    let Some(id) = tab
                        .strip_prefix("giverny-")
                        .and_then(|n| n.parse::<u64>().ok())
                        .map(TabId)
                    else {
                        continue;
                    };
                    if let Some(tab) = self.ws.tab_mut(id) {
                        let moved = tab.cwd.as_deref() != Some(Path::new(&cwd));
                        if moved || tab.wsl_distro.as_deref() != Some(distro.as_str()) {
                            tab.cwd = Some(PathBuf::from(cwd));
                            tab.wsl_distro = Some(distro);
                            self.state_dirty = true;
                        }
                    }
                }
            }
        }
        if self.wsl_cwd_rx.is_some() || self.last_wsl_probe.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_wsl_probe = Instant::now();
        if wsl::distros().is_empty() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        if std::thread::Builder::new()
            .name("giverny wsl cwd".into())
            .spawn(move || {
                let _ = tx.send(wsl::tab_cwds());
            })
            .is_ok()
        {
            self.wsl_cwd_rx = Some(rx);
        }
    }

    /// When this tab's usage window reopens, if it is waiting for one.
    /// `None` for every tab that is not.
    pub fn limited_until(&self, id: TabId) -> Option<jiff::Timestamp> {
        self.limited.get(&id).and_then(|w| w.reopens)
    }

    /// Is this tab stopped by a usage limit at all, reset time known or not?
    pub fn is_limited(&self, id: TabId) -> bool {
        self.limited.contains_key(&id)
    }

    /// How the rail groups tabs right now.
    pub fn rail_view(&self) -> giverny_core::state::RailView {
        self.layout.rail_view
    }

    /// Is this repository's group folded away? `None` is the group for tabs
    /// in no repository, which is a group like any other.
    pub fn repo_collapsed(&self, repo: Option<&Path>) -> bool {
        match repo {
            Some(path) => self.layout.collapsed_repos.iter().any(|p| p == path),
            None => self
                .layout
                .collapsed_repos
                .iter()
                .any(|p| p.as_os_str().is_empty()),
        }
    }

    /// Which repository each tab is in, for the rail's by-repository view.
    ///
    /// Off the UI thread, because for a tab inside WSL every one of these
    /// lookups is a round trip over the distribution's share — walking a few
    /// ancestors for each of a dozen tabs, twice a minute, is not something to
    /// do between two frames. Cached by directory: tabs share them, and the
    /// answer only changes when a tab moves.
    fn refresh_repos(&mut self) {
        let answer = self.repo_rx.as_ref().map(|rx| rx.try_recv());
        match answer {
            Some(Err(crossbeam_channel::TryRecvError::Empty)) | None => {}
            Some(Err(crossbeam_channel::TryRecvError::Disconnected)) => self.repo_rx = None,
            Some(Ok(found)) => {
                self.repo_rx = None;
                for (id, dir, repo, branch) in found {
                    self.repo_cache.insert(dir, repo.clone());
                    if let Some(tab) = self.ws.tab_mut(id) {
                        if tab.git_repo != repo {
                            tab.git_repo = repo;
                            self.state_dirty = true;
                        }
                        // Only ever had a branch once a tab had been focused,
                        // because the /proc refresh that set it runs for the
                        // active tab alone.
                        tab.git_branch = branch;
                    }
                }
            }
        }
        if self.repo_rx.is_some() {
            return;
        }
        let dirs: Vec<(TabId, PathBuf)> = self
            .ws
            .tabs
            .iter()
            .filter_map(|t| Some((t.id, self.local_path(t)?)))
            .collect();
        if dirs.is_empty() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let known = self.repo_cache.clone();
        if std::thread::Builder::new()
            .name("giverny repo sweep".into())
            .spawn(move || {
                // One HEAD read per repository rather than per tab: every tab
                // in a checkout is on the same branch. Repository roots are
                // remembered between sweeps; a branch is the thing that
                // changes, so it never is.
                let mut branches: HashMap<PathBuf, Option<String>> = HashMap::new();
                let found = dirs
                    .into_iter()
                    .map(|(id, dir)| {
                        let repo = match known.get(&dir) {
                            Some(hit) => hit.clone(),
                            None => giverny_core::git::repo_root(&dir),
                        };
                        let branch = repo.as_ref().and_then(|root| {
                            branches
                                .entry(root.clone())
                                .or_insert_with(|| giverny_core::git::branch_of(root))
                                .clone()
                        });
                        (id, dir, repo, branch)
                    })
                    .collect();
                let _ = tx.send(found);
            })
            .is_ok()
        {
            self.repo_rx = Some(rx);
        }
    }

    /// A tab's directory as *this* machine can open it.
    ///
    /// For a tab inside WSL the two differ: the shell reports `/home/x/proj`,
    /// which Windows reaches as `\\wsl.localhost\<distro>\home\x\proj` and
    /// otherwise cannot see at all — which is why a WSL tab has never shown a
    /// git branch.
    fn local_path(&self, tab: &giverny_core::tabs::Tab) -> Option<PathBuf> {
        let cwd = tab.cwd.clone()?;
        match &tab.wsl_distro {
            Some(distro) if cwd.to_str().is_some_and(|c| c.starts_with('/')) => {
                Some(wsl::unc_path(distro, cwd.to_str()?))
            }
            _ => Some(cwd),
        }
    }

    /// Refresh cwd (via /proc) and git branch for one tab.
    fn refresh_tab_info(&mut self, id: TabId) {
        let pid = self
            .rt
            .get(&id)
            .and_then(|rt| rt.session.as_ref())
            .and_then(|s| s.child_pid);
        #[cfg(target_os = "linux")]
        if let Some(pid) = pid
            && let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd"))
            && let Some(tab) = self.ws.tab_mut(id)
        {
            tab.cwd = Some(cwd);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        let local = self.ws.tab(id).and_then(|t| self.local_path(t));
        let branch = local.as_deref().and_then(giverny_core::git::branch_of);
        if let Some(tab) = self.ws.tab_mut(id) {
            tab.git_branch = branch;
        }
    }

    /// Hot-reload `config.toml` when it changes on disk.
    fn reload_config_if_changed(&mut self, ctx: &egui::Context) {
        if self.last_cfg_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_cfg_check = Instant::now();
        let mtime = config_mtime(&self.paths);
        if mtime == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = mtime;
        self.reload_config(ctx);
    }

    /// Read `config.toml` again and apply it. A file that does not parse
    /// changes nothing: the running settings stay.
    fn reload_config(&mut self, ctx: &egui::Context) {
        match config::load_checked(self.paths.base(), &self.cfg) {
            Ok(parsed) => {
                let read = accounts_readable(&parsed);
                self.apply_config(ctx, parsed.config);
                if read && std::mem::take(&mut self.accounts_unset) {
                    tracing::info!("config.toml parses again: setting up Claude accounts");
                    set_up_accounts(&mut self.claude, &self.cfg, true);
                }
            }
            Err(err) => tracing::error!("config.toml ignored ({err}); keeping previous settings"),
        }
    }

    /// Put a theme on the grid, on every open session, and on the chrome.
    fn adopt_theme(&mut self, ctx: &egui::Context, theme: Theme) {
        self.shared.set_theme(theme.clone());
        for rt in self.rt.values() {
            if let Some(session) = &rt.session {
                *session.shared.theme.write() = theme.clone();
                session.mark_dirty();
            }
        }
        // The chrome is themed too, so the rail does not stay Monet-blue
        // around a Gruvbox grid.
        self.chrome = chrome::Chrome::from_theme(&theme);
        self.chrome.apply(ctx, &theme, self.shared.opacity);
    }

    /// Adopt a freshly loaded config, applying what can be applied live.
    fn apply_config(&mut self, ctx: &egui::Context, cfg: config::Config) {
        if cfg.theme.name != self.cfg.theme.name {
            self.adopt_theme(ctx, theme_for(&cfg.theme.name));
        }
        if cfg.font.size != self.cfg.font.size {
            self.shared.set_font_size(cfg.font.size);
        }
        if cfg.font.family != self.cfg.font.family {
            tracing::info!("font family changed — restart Giverny to apply");
        }
        if cfg.claude.auto_mode != self.cfg.claude.auto_mode {
            self.claude.set_auto_mode(cfg.claude.auto_mode);
        }
        let opacity = opacity_for(self.see_through, &cfg);
        if opacity != self.shared.opacity {
            self.shared.opacity = opacity;
            self.chrome.apply(ctx, &self.shared.theme.clone(), opacity);
        } else if cfg.window.opacity != self.cfg.window.opacity
            && cfg.window.translucent()
            && !self.see_through
        {
            tracing::info!("window.opacity: restart Giverny to make the window see-through");
        }
        self.cfg = cfg;
        tracing::info!("config reloaded");
    }

    /// Dropping files types their paths into the active tab, the way every
    /// other terminal behaves — and the way you hand Claude an image.
    ///
    /// Routed through the paste encoder, so the text is bracketed-paste
    /// wrapped and escape-sanitized: a filename is untrusted input, and a
    /// path arriving as if typed must not be able to run anything.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        self.deliver_drop(ctx, paths, None);
        self.handle_wayland_drag(ctx);
    }

    /// Type dropped paths into a tab: the one under the pointer when we know
    /// where the drag was (Wayland — we track it ourselves), otherwise the
    /// active one.
    fn deliver_drop(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>, at: Option<egui::Pos2>) {
        if paths.is_empty() {
            return;
        }
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let text = giverny_term::input::dropped_paths_text(&paths);
        let Some(id) = at.and_then(|p| self.tab_at(p)).or(self.ws.active) else {
            return;
        };
        // Dropping on an inactive tab means you want that tab: switch to it,
        // spawning its shell if it was never focused.
        if self.ws.active != Some(id) {
            self.apply(ctx, Action::Select(id));
        }
        if let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) {
            session.write(giverny_term::input::encode_paste(&text, session.mode()));
            session.note_user_input();
            self.focus_terminal = true;
        }
    }

    /// The tab whose rail row contains a point, if any.
    pub fn tab_at(&self, pos: egui::Pos2) -> Option<TabId> {
        self.row_rects
            .iter()
            .find(|(rect, _)| rect.contains(pos))
            .map(|&(_, id)| id)
    }

    /// Wayland: drain the drag thread. Positions arrive in surface-local
    /// logical pixels; egui works in points, which differ by the zoom factor.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    fn handle_wayland_drag(&mut self, ctx: &egui::Context) {
        let Some(dnd) = &self.dnd else { return };
        let events = dnd.poll();
        if events.is_empty() {
            return;
        }
        let (native, ppp) =
            ctx.input(|i| (i.viewport().native_pixels_per_point, i.pixels_per_point()));
        let to_points = |(x, y): (f32, f32)| {
            let scale = native.unwrap_or(ppp) / ppp;
            egui::Pos2::new(x * scale, y * scale)
        };
        for event in events {
            match event {
                wayland_dnd::DragEvent::Enter(at) | wayland_dnd::DragEvent::Motion(at) => {
                    self.drag_hover = at.map(to_points);
                }
                wayland_dnd::DragEvent::Leave => self.drag_hover = None,
                wayland_dnd::DragEvent::Drop(paths) => {
                    let at = self.drag_hover.take();
                    self.deliver_drop(ctx, paths, at);
                }
            }
        }
    }

    #[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
    fn handle_wayland_drag(&mut self, _ctx: &egui::Context) {}

    fn claude_env(&self) -> Vec<(String, String)> {
        claude_env(&self.cfg.claude)
    }

    /// Notice window and rail resizes so they persist with everything else.
    ///
    /// Read back from the window rather than tracked at the drag, because the
    /// window manager has the last word: a tiled or snapped window ends up a
    /// size nobody asked for, and that is still the size to reopen at.
    fn track_layout(&mut self, ctx: &egui::Context) {
        let before = self.layout.clone();
        let (maximized, viewport_rect) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.maximized.unwrap_or(false) || vp.fullscreen.unwrap_or(false),
                i.viewport_rect(),
            )
        });
        // Maximised by hand on WSLg (#78) is maximised all the same.
        let maximized = maximized || (self.frameless && self.maximize.is_on(ctx));
        // Deliberately not `viewport().inner_rect`: it is derived from the
        // window's *position*, which Wayland never reports, so it is None on
        // the primary platform. The egui surface is the window's inner area
        // everywhere; scaled by the zoom factor it is the logical size
        // `with_inner_size` wants back.
        let size = viewport_rect.size() * ctx.zoom_factor();
        self.layout.maximized = maximized;
        // A maximized window's size is the screen's, not the user's choice —
        // storing it would reopen unmaximized at monitor size.
        if !maximized && size.x > 0.0 && size.y > 0.0 {
            self.layout.window = Some([size.x, size.y]);
        }
        if let Some(rail) = egui::PanelState::load(ctx, egui::Id::new("rail")) {
            self.layout.rail_width = Some(rail.size().x);
        }
        // Ctrl+± is egui's own zoom; read it back rather than intercepting
        // the chord, which the terminal widget also answers to.
        let zoom = ctx.zoom_factor();
        if self.layout.zoom.is_some() || (zoom - 1.0).abs() > 0.001 {
            self.layout.zoom = Some(zoom);
        }

        let moved = |a: Option<f32>, b: Option<f32>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() >= 1.0,
            (a, b) => a.is_some() != b.is_some(),
        };
        if self.frameless {
            self.layout.work_area = self.maximize.learned();
        }
        let changed = self.layout.maximized != before.maximized
            || self.layout.work_area != before.work_area
            || self.layout.zoom != before.zoom
            || moved(self.layout.rail_width, before.rail_width)
            || moved(
                self.layout.window.map(|w| w[0]),
                before.window.map(|w| w[0]),
            )
            || moved(
                self.layout.window.map(|w| w[1]),
                before.window.map(|w| w[1]),
            );
        if changed {
            self.state_dirty = true;
        }
    }

    /// Ctrl+±/0 changes the font size live. Write it back to `config.toml`,
    /// which is the single place the size lives — it used to be persisted
    /// separately in the state file too, where it silently outranked the
    /// config and the settings screen.
    fn persist_font_size(&mut self) {
        let live = self.shared.font_size;
        if (live - self.cfg.font.size).abs() < 0.01 {
            return;
        }
        let Some(def) = giverny_core::settings::by_key("font.size") else {
            return;
        };
        let value = giverny_core::settings::Value::Float(live as f64);
        match giverny_core::settings::write(self.paths.base(), def, &value) {
            Ok(()) => {
                self.cfg.font.size = live;
                self.cfg_mtime = config_mtime(&self.paths);
            }
            Err(err) => tracing::warn!("could not persist font size: {err:#}"),
        }
    }

    /// Remember what each tab is running, so a restart can bring it back.
    fn track_foreground(&mut self) {
        let seen: Vec<(TabId, Option<String>)> = self
            .rt
            .iter()
            .filter_map(|(&id, rt)| {
                let pid = rt.session.as_ref()?.child_pid?;
                Some((id, giverny_core::procs::foreground_command(pid)))
            })
            .collect();
        for (id, cmd) in seen {
            if let Some(tab) = self.ws.tab_mut(id)
                && tab.foreground != cmd
            {
                tab.foreground = cmd;
                self.state_dirty = true;
            }
        }
    }

    fn periodic_refresh(&mut self, ctx: &egui::Context) {
        self.reload_config_if_changed(ctx);
        if let Some(welcome) = self.welcome.take() {
            self.open_welcome(ctx, &welcome);
        }
        // Rouen's light moves with the clock. A minute is finer than anyone
        // can see, and an idle window still has to wake up for it.
        if self.cfg.theme.name == "rouen" {
            if self.theme_tick.elapsed() >= std::time::Duration::from_secs(60) {
                self.theme_tick = std::time::Instant::now();
                self.adopt_theme(ctx, theme_for("rouen"));
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(60));
        }
        if self.state_dirty && self.last_save.elapsed() > Duration::from_secs(2) {
            self.save_state();
        }
        if self.last_info_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_info_refresh = Instant::now();
        self.snapshot_stalest_tab();
        if let Some(rx) = &self.update_rx {
            while let Ok(found) = rx.try_recv() {
                // A dismissed banner stays dismissed until a *newer* version
                // than the one waved away turns up.
                if found.as_ref().map(|f| &f.version) != self.update.as_ref().map(|u| &u.version) {
                    self.update_dismissed = false;
                }
                self.update = found;
            }
        }
        // While an install is running in its tab, watch for the binary being
        // replaced: that is the moment a restart has something to pick up.
        if self.update_job.is_some() && !self.update_installed {
            self.update_installed =
                update::binary_mtime().is_some_and(|at| Some(at) != self.exe_mtime);
        }
        self.persist_font_size();
        self.track_foreground();
        self.follow_update_job();
        self.probe_wsl_cwds();
        self.refresh_repos();
        // Ask Claude Code to refresh accounts whose numbers have aged out.
        self.claude
            .refresh_stale_usage(self.cfg.usage.refresh_minutes, false);
        if let Some(id) = self.ws.active {
            self.refresh_tab_info(id);
        }
    }

    /// Notice an install that ended without the binary changing: its tab was
    /// closed, or its shell is back at the prompt after running something
    /// (or never visibly ran it). Either way the update button comes back, so
    /// it can be tried again.
    fn follow_update_job(&mut self) {
        if self.update_installed {
            return;
        }
        let Some(job) = &mut self.update_job else {
            return;
        };
        let Some(tab) = self.ws.tab(job.tab) else {
            self.update_job = None;
            return;
        };
        if tab.foreground.is_some() {
            job.seen_running = true;
        } else if job.seen_running || job.started.elapsed() > UPDATE_UNSEEN_GRACE {
            self.update_job = None;
            self.update_stalled = true;
        }
    }

    /// Restart the full-screen program this tab was running, when it is one
    /// Giverny is allowed to start by itself.
    fn queue_app_restore(&mut self, id: TabId) {
        let Some(tab) = self.ws.tab(id) else { return };
        // A tab with a Claude session resumes that instead.
        if tab.claude_session.is_some() {
            return;
        }
        let Some(cmd) = tab.foreground.clone() else {
            return;
        };
        if !giverny_core::procs::is_restorable(&cmd, &self.cfg.behavior.restore_apps) {
            tracing::info!("tab {id:?}: not restarting {cmd:?} (not on the restore list)");
            return;
        }
        let mut bytes = cmd.into_bytes();
        bytes.push(b'\r');
        self.pending_inject.push((
            Instant::now() + Duration::from_millis(1200),
            id,
            Inject::Raw(bytes),
        ));
    }

    /// Queue the auto-resume command for a freshly restored tab.
    fn queue_resume(&mut self, id: TabId) {
        let Some(sid) = self.session_to_resume(id) else {
            return;
        };
        if let Some(cmd) = self.resume_command(&sid, id) {
            tracing::info!("tab {id:?}: resuming claude session {sid}");
            self.pending_inject.push((
                Instant::now() + Duration::from_millis(1300),
                id,
                Inject::Raw(cmd),
            ));
        }
    }

    /// Which conversation this tab should come back to.
    ///
    /// Normally the id captured while the session was live. Failing that, the
    /// command the tab was last running: a `claude --resume <id>` names its
    /// own conversation, and that is the one thing still written down when a
    /// crash beats everything else to it. Adopted onto the tab, so the next
    /// save has it even if this resume never runs.
    fn session_to_resume(&mut self, id: TabId) -> Option<String> {
        let tab = self.ws.tab(id)?;
        if let Some(sid) = tab.claude_session.clone() {
            return Some(sid);
        }
        tracing::debug!("tab {id:?}: no conversation recorded; checking its last command");
        let mined = tab
            .foreground
            .as_deref()
            .and_then(giverny_core::procs::resume_session_of)
            .map(str::to_string)?;
        tracing::info!("tab {id:?}: recovered session {mined} from its last command");
        self.ws.tab_mut(id)?.claude_session = Some(mined.clone());
        self.state_dirty = true;
        Some(mined)
    }

    /// Build the shell command that resumes `sid` in tab `id`, with every
    /// guard: id validation, double-resume protection, transcript lookup
    /// across profiles (self-heals a lost account association), and the
    /// conversation's own recorded cwd (`claude --resume` only finds a
    /// session from the directory it ran in).
    fn resume_command(&self, sid: &str, id: TabId) -> Option<Vec<u8>> {
        let tab = self.ws.tab(id)?;
        let all_dirs: Vec<PathBuf> = self
            .claude
            .profiles
            .iter()
            .map(|p| p.config_dir.clone())
            .collect();
        match resume_plan(
            sid,
            tab.cwd.as_deref(),
            tab.claude_config_dir.as_deref(),
            &all_dirs,
        ) {
            ResumePlan::Ready { command, .. } => Some(command.into_bytes()),
            ResumePlan::Malformed => {
                tracing::warn!("tab {id:?}: malformed session id {sid:?} — not resuming");
                None
            }
            ResumePlan::LiveElsewhere => {
                tracing::info!("claude session {sid} already live elsewhere — not resuming");
                None
            }
            ResumePlan::NoTranscript => {
                tracing::info!("tab {id:?}: no transcript for session {sid} — skipping resume");
                None
            }
        }
    }

    fn process_pending(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending_inject.len() {
            if self.pending_inject[i].0 <= now {
                let (_, id, inject) = self.pending_inject.remove(i);
                let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) else {
                    continue;
                };
                // The user took over — automated typing stands down. Except
                // a wake, which is aimed at a tab they have worked in and
                // carries its own, narrower test.
                if session.had_user_input() && !matches!(inject, Inject::Wake { .. }) {
                    continue;
                }
                match inject {
                    Inject::Raw(bytes) => session.write(bytes),
                    Inject::Wake { bytes, seq } => {
                        if session.input_seq() == seq {
                            session.write(bytes);
                        }
                    }
                    Inject::CwdFix(expected) => {
                        let actual = session.proc_cwd();
                        if actual.as_ref().is_some_and(|a| *a != expected) && expected.is_dir() {
                            session.write(format!("cd \"{}\"\r", expected.display()).into_bytes());
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
        if !self.pending_inject.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// One Ctrl+Tab press: start a walk through the recency order, or step
    /// further into the one already running. The tab is shown as it is
    /// stepped past, but nothing is recorded — `end_switch` does that when
    /// Ctrl comes up, so walking past a tab does not count as using it.
    fn switch_recent(&mut self, delta: i32) {
        if self.switcher.is_none() {
            let Some(from) = self.ws.active else { return };
            let order = self.ws.recent_order();
            if order.len() < 2 {
                return;
            }
            self.switcher = Some(Switcher {
                from,
                order,
                index: 0,
            });
        }
        let Some(sw) = self.switcher.as_mut() else {
            return;
        };
        let len = sw.order.len() as i32;
        sw.index = (sw.index as i32 + delta).rem_euclid(len) as usize;
        let target = sw.order[sw.index];
        self.ws.preview_active(target);
        self.reveal_terminal();
        self.state_dirty = true;
    }

    /// How a tab reaches Claude Code: which shell opens, which account the
    /// tab is on, and the environment that carries both.
    ///
    /// On Windows this is where the WSL boundary is crossed. A tab that opens
    /// in a distribution runs a Claude Code that can only see the account
    /// *inside* it, so the account is named in unix terms for the session and
    /// in Windows terms for everything Giverny stores; and a Windows
    /// environment does not reach a process in a distribution at all unless
    /// `%WSLENV%` lists it, which is how the tab identity the hooks report
    /// gets there.
    fn tab_shape(&self, profile_dir: Option<PathBuf>, was_in: Option<&Path>) -> TabShape {
        let mut env = self.claude_env();
        let shell = pty::windows_shell(self.cfg.behavior.windows_shell.as_str());

        // The account the category names wins over the shell preference: an
        // account inside a distribution is only reachable from that
        // distribution, so that is where the tab opens.
        let distro = profile_dir
            .as_deref()
            .and_then(wsl::split_unc)
            .map(|(distro, _)| distro)
            .or_else(|| {
                pty::opens_wsl(self.cfg.behavior.windows_shell.as_str())
                    .then(wsl::default_distro)
                    .flatten()
            });
        let Some(distro) = distro else {
            // A default account is named by not naming it — the same rule the
            // distribution branch below follows, and for the same reason.
            // `GIVERNY_PROFILE_DIR` carries the attribution instead, so a hook
            // still says which account its session ran under.
            let (config_dir, profile_dir) = match &profile_dir {
                Some(dir) if !giverny_claude::profiles::must_be_named(dir) => (None, profile_dir),
                _ => (profile_dir.clone(), profile_dir),
            };
            if let Some(dir) = &profile_dir {
                env.push(("GIVERNY_PROFILE_DIR".into(), dir.display().to_string()));
            }
            return TabShape {
                shell,
                config_dir,
                env,
                in_wsl: false,
            };
        };

        // Everything below names the account itself, because none of the
        // values a session inside a distribution needs are the ones
        // `SpawnCfg` would derive: the path has to be the unix one, and for
        // the distribution's own default account the right value is no value.
        let account = profile_dir.or_else(|| wsl::account_dir(&distro));
        if let Some((dir, (_, unix))) = account
            .as_deref()
            .and_then(|d| wsl::split_unc(d).map(|split| (d, split)))
        {
            // Naming the account Claude Code would have picked anyway is not
            // the harmless no-op it looks like: `CLAUDE_CONFIG_DIR` also
            // moves where Claude Code keeps its identity — inside the
            // directory instead of beside it — so a session handed the path
            // of its own default account comes up logged out. Say nothing,
            // and it finds that account by itself.
            if giverny_claude::profiles::must_be_named(dir) {
                env.push(("CLAUDE_CONFIG_DIR".into(), unix));
            }
        }
        if let Some(account) = &account {
            // Which account the tab is on, in the terms Giverny stores
            // accounts in. A hook fired inside the distribution carries it
            // back out, which is the only way a session that was never told a
            // config dir can be attributed to an account at all.
            env.push(("GIVERNY_PROFILE_DIR".into(), account.display().to_string()));
        }
        let shared: Vec<&str> = env
            .iter()
            .map(|(name, _)| name.as_str())
            // Everything we actually set that the other side reads — and
            // only what we set, since listing a name we did not set is what
            // put an empty CLAUDE_CONFIG_DIR in front of Claude Code.
            .filter(|name| name.starts_with("GIVERNY_") || name.starts_with("CLAUDE_"))
            // These two come from the spawn itself rather than from here.
            .chain(["GIVERNY_TAB_ID", "GIVERNY_NONCE"])
            .collect();
        env.push((
            "WSLENV".into(),
            wslenv(std::env::var("WSLENV").ok(), &shared),
        ));
        TabShape {
            shell: Some(wsl_shell(
                &distro,
                wsl::default_distro().as_deref(),
                wsl_start_dir(&distro, was_in).as_deref(),
            )),
            config_dir: None,
            env,
            in_wsl: true,
        }
    }

    /// Picking a tab means "show me that tab": the settings screen and the
    /// key list take the terminal's place, so a tab clicked in the rail while
    /// one of them is open used to look like a click that did nothing — the
    /// only way back was the button that opened it.
    fn reveal_terminal(&mut self) {
        self.settings = None;
        self.keys_overlay = None;
        self.focus_terminal = true;
    }

    /// Commit a Ctrl+Tab walk: the tab it landed on becomes the current one,
    /// and only now does it count as seen.
    fn end_switch(&mut self) {
        let Some(sw) = self.switcher.take() else {
            return;
        };
        self.ws.commit_switch(sw.from);
        if let Some(id) = self.ws.active {
            self.refresh_tab_info(id);
            self.claude.mark_viewed(id);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) -> Vec<Action> {
        let mut actions = Vec::new();
        ctx.input_mut(|i| {
            let cs = Modifiers::CTRL | Modifiers::SHIFT;
            if i.consume_key(cs, Key::T)
                && let Some(cat) = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id))
            {
                actions.push(Action::NewTab {
                    category: cat,
                    cwd: None,
                });
            }
            if i.consume_key(cs, Key::W)
                && let Some(id) = self.ws.active
            {
                actions.push(Action::CloseTab(id));
            }
            if i.consume_key(cs, Key::A) {
                actions.push(Action::JumpAttention);
            }
            if i.consume_key(cs, Key::P) {
                actions.push(Action::TogglePalette);
            }
            if i.consume_key(Modifiers::CTRL, Key::Comma) {
                actions.push(Action::ToggleSettings);
            }
            // F1 anywhere, and Ctrl+Shift+/ for muscle memory from editors.
            if i.consume_key(Modifiers::NONE, Key::F1) || i.consume_key(cs, Key::Slash) {
                actions.push(Action::ToggleKeys);
            }
            // Ctrl+Tab walks recency, not rail order: one press is "back to
            // the tab I came from", and holding Ctrl keeps going back.
            if i.consume_key(cs, Key::Tab) {
                actions.push(Action::SwitchRecent(-1));
            }
            if i.consume_key(Modifiers::CTRL, Key::Tab) {
                actions.push(Action::SwitchRecent(1));
            }
            if i.consume_key(Modifiers::CTRL, Key::PageDown) {
                actions.push(Action::Cycle(1));
            }
            if i.consume_key(Modifiers::CTRL, Key::PageUp) {
                actions.push(Action::Cycle(-1));
            }
            if i.consume_key(Modifiers::NONE, Key::F2)
                && let Some(id) = self.ws.active
            {
                actions.push(Action::StartRename(RenameTarget::Tab(id)));
            }
        });
        actions
    }
}

/// The terminal `id` has lost the keyboard to something that is not meant
/// to hold it, and should take it back: anything but a text field, while no
/// `overlay` reading keys of its own is open.
fn terminal_lost_keys(ctx: &egui::Context, id: egui::Id, overlay: bool) -> bool {
    !overlay && !ctx.text_edit_focused() && !ctx.memory(|m| m.has_focus(id))
}

/// Frames drawn per second, logged every ten seconds at debug level
/// (`RUST_LOG=giverny=debug`), with the passes egui ran for them and what
/// asked for the last one. On a software renderer every frame is CPU, so
/// this is the number that explains a busy process (#43).
fn count_frame(ctx: &egui::Context) {
    use std::sync::Mutex;
    static FRAMES: Mutex<Option<(Instant, u32, u32)>> = Mutex::new(None);
    let Ok(mut slot) = FRAMES.lock() else { return };
    let (since, frames, passes) = slot.get_or_insert((Instant::now(), 0, 0));
    *passes += 1;
    if ctx.current_pass_index() == 0 {
        *frames += 1;
    }
    let elapsed = since.elapsed();
    if elapsed >= Duration::from_secs(10) {
        let secs = elapsed.as_secs_f64();
        tracing::debug!(
            "frames: {:.1}/s ({:.1} passes/s, predicted {:.0} ms each) over {secs:.0}s, \
             the last asked for by {:?}",
            *frames as f64 / secs,
            *passes as f64 / secs,
            ctx.input(|i| i.predicted_dt) * 1000.0,
            ctx.repaint_causes()
        );
        *slot = Some((Instant::now(), 0, 0));
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        translucency::clear_color(self.see_through)
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        count_frame(ui.ctx());
        // Documentation capture (GIVERNY_CAPTURE); no-op otherwise.
        if let Some(cap) = &mut self.capture {
            cap.on_frame(ui.ctx());
            if cap.done() {
                self.capture = None;
            }
        }
        self.drain_events();

        let ctx = ui.ctx().clone();
        // A close asked for through the window and one asked for with a
        // signal end the same way, but only the first arrives as an event.
        // Both are written down here: the state file's clean-shutdown marker
        // is only true if one of them happened.
        if !self.closing
            && (self.terminating.load(Ordering::Relaxed)
                || ctx.input(|i| i.viewport().close_requested()))
        {
            self.closing = true;
            // Save now rather than leaving it to the drop. A signal at logout
            // races the compositor going away, and a frame that never
            // finishes writes nothing.
            self.persist_all();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.periodic_refresh(&ctx);
        self.handle_dropped_files(&ctx);
        self.process_pending(&ctx);

        // Claude awareness: hooks + registry + usage.
        let shell_pids: HashMap<TabId, u32> = self
            .rt
            .iter()
            .filter_map(|(&id, rt)| {
                rt.session
                    .as_ref()
                    .and_then(|s| s.child_pid)
                    .map(|p| (id, p))
            })
            .collect();
        let titles: HashMap<TabId, String> = self
            .ws
            .tabs
            .iter()
            .map(|t| (t.id, t.display_title(&self.cfg.titles)))
            .collect();
        // Typing in a tab counts as attending to it: declining a permission
        // prompt (Escape) emits no hook, so nothing else would clear the flag.
        for (&id, rt) in &self.rt {
            let Some(session) = &rt.session else { continue };
            let seq = session.input_seq();
            if self
                .input_seen
                .insert(id, seq)
                .is_some_and(|prev| prev != seq)
            {
                self.claude.mark_attended(id);
            }
        }

        let effects = self.claude.tick(&shell_pids, self.ws.active, &titles);
        self.show_attention(&ctx, frame);
        self.watch_for_limits();
        for (id, session, config_dir) in effects.captured {
            if let Some(tab) = self.ws.tab_mut(id) {
                tab.claude_session = session;
                if config_dir.is_some() {
                    tab.claude_config_dir = config_dir;
                }
                self.state_dirty = true;
            }
        }
        for (summary, body) in effects.notify {
            desktop_notify(summary, body);
        }
        // Heartbeat: egui only repaints on demand, so without this the
        // registry scan (and therefore state transitions for tabs whose
        // output isn't waking the UI) would stall while the window idles.
        //
        // A working tab no longer shortens it. The spinners wake the window
        // themselves, at their own step and only while they are on screen
        // (`rail::keep_animating`); a timer here repainted everything at
        // ~8 fps for as long as any tab worked, seen or not (#43). On the
        // animation clock's grid, so it shares a frame with them when they
        // run.
        ctx.request_repaint_after(
            giverny_term::pace::until_next_tick(true, ctx.input(|i| i.predicted_dt))
                + Duration::from_millis(500),
        );

        let mut actions = self.shortcuts(&ctx);

        #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
        if let Some(cursors) = &mut self.cursors {
            cursors.keep();
        }
        if self.frameless && !ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            self.maximize.update(&ctx);
            let maximized = self.maximize.is_on(&ctx);
            titlebar::resize_edges(&ctx, maximized);
            titlebar::outline(&ctx, &self.chrome, maximized);
            let title = if self.attention > 0 {
                format!("Giverny ({})", self.attention)
            } else {
                "Giverny".to_string()
            };
            egui::Panel::top("titlebar")
                .exact_size(titlebar::HEIGHT)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    titlebar::show(ui, &title, &self.chrome, &mut self.maximize)
                });
        }

        egui::Panel::left("rail")
            .resizable(true)
            .default_size(
                self.layout
                    .rail_width_in(RAIL_MIN..=RAIL_MAX)
                    .unwrap_or(240.0),
            )
            .size_range(RAIL_MIN..=RAIL_MAX)
            .show(ui, |ui| {
                actions.extend(rail::show(self, ui));
            });
        self.track_layout(&ctx);

        for action in actions.drain(..) {
            self.apply(&ctx, action);
        }

        // A see-through window paints the area around the grid itself, so
        // that no pixel has two translucent layers on it: the panel's fill
        // under the grid's would show the desktop through both at once.
        let opacity = self.shared.opacity;
        let mut central = egui::CentralPanel::default();
        if opacity < 1.0 {
            central =
                central.frame(egui::Frame::central_panel(ui.style()).fill(Color32::TRANSPARENT));
        }
        let mut backdrop = None;
        let mut grid_rect = None;
        let mut solid = false;
        let panel = central.show(ui, |ui| {
            if opacity < 1.0 {
                backdrop = Some((ui.painter().add(egui::Shape::Noop), ui.painter().clone()));
            }
            // Settings take the terminal's place, so the rail stays visible
            // and changes to it can be watched landing. The shell behind is
            // untouched and keeps running.
            if self.settings.is_some() {
                // Solid: a screen of text and controls read against the
                // wallpaper would be the hardest thing in the app to read.
                solid = true;
                let acts = settings_ui::settings_ui(self, ui);
                actions.extend(acts);
                return;
            }
            let Some(active) = self.ws.active else {
                ui.centered_and_justified(|ui| {
                    ui.label("no tabs — Ctrl+Shift+T opens one");
                });
                return;
            };

            // Category accent strip above the pane: you always know where you are.
            let accent = self
                .ws
                .tab(active)
                .and_then(|t| self.ws.category(t.category))
                .map(|c| self.chrome.category(c.color_index))
                .unwrap_or(Color32::GRAY);
            let (strip, _) = ui.allocate_exact_size(
                egui::Vec2::new(ui.available_width(), 3.0),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(strip, 0.0, accent);

            // Files hovering over the window: say what a drop will do, so a
            // drag is not a guess. On X11 winit counts the files but reports
            // no position, so the destination is the active tab; on Wayland
            // we track the drag ourselves and can name the tab under the
            // pointer instead.
            let hovering = ctx.input(|i| i.raw.hovered_files.len());
            if hovering > 0 || self.drag_hover.is_some() {
                let target = self
                    .drag_hover
                    .and_then(|p| self.tab_at(p))
                    .unwrap_or(active);
                let into = self
                    .ws
                    .tab(target)
                    .map(|t| t.display_title(&self.cfg.titles))
                    .unwrap_or_default();
                let what = match hovering {
                    0 => "drop into".to_string(),
                    1 => "drop 1 path into".to_string(),
                    n => format!("drop {n} paths into"),
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{what}  {into}"))
                            .font(egui::FontId::monospace(11.0))
                            .color(self.chrome.accent),
                    );
                });
            }

            let exited = self.ws.tab(active).is_some_and(|t| t.exited);
            if exited {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("process exited")
                            .color(Color32::from_rgb(0xd9, 0xb5, 0x5f)),
                    );
                    if ui.small_button("respawn").clicked() {
                        actions.push(Action::Respawn(active));
                    }
                });
            }

            // Lazy restore: a tab from a previous run spawns its shell on
            // first focus, pre-seeded with its saved scrollback.
            if !self.rt.contains_key(&active) && !self.ws.tab(active).is_some_and(|t| t.exited) {
                let preseed = state::load_snapshot(&self.paths, active)
                    .map(|dump| format!("{dump}\x1b[0m\x1b[2m── restored ──\x1b[0m\r\n\r\n"));
                self.spawn_session(&ctx, active, preseed);
                // The tab had a live Claude conversation — resume it; or a
                // full-screen app worth starting again.
                self.queue_resume(active);
                self.queue_app_restore(active);
            }

            if let Some(rt) = self.rt.get_mut(&active) {
                if let Some(session) = &mut rt.session {
                    // The wheel over the closed prompt bar scrolls the
                    // terminal under it.
                    rt.view.wheel_through([prompt_bar::layer(active)]);
                    let response = rt.view.show(ui, &mut self.shared, session);
                    grid_rect = Some(response.rect);
                    // Typing goes to the terminal. egui drops a widget's
                    // focus on any press outside it — the rail, the
                    // taskbar, a header button — and the keys typed after
                    // that went nowhere until the terminal was clicked
                    // again. So it takes the keyboard back whenever nothing
                    // else is meant to hold it: a text field (the search
                    // bar, a settings input) or an overlay that reads keys
                    // of its own. Only when it has lost it, since each
                    // request interrupts IME input.
                    let overlay = self.palette.is_some()
                        || self.session_picker.is_some()
                        || self.keys_overlay.is_some()
                        || self.rename.is_some();
                    if self.focus_terminal || terminal_lost_keys(&ctx, response.id, overlay) {
                        response.request_focus();
                        self.focus_terminal = false;
                    }
                    // The prompt whose turn is in view, once its own row
                    // has scrolled out of sight.
                    let pinned = self.claude.prompts_of(active).and_then(|history| {
                        rt.prompt_bar
                            .get(session.content_seq(), history, || {
                                let rows = session.viewport_rows();
                                prompt_bar::owner(history, &rows, |matches| {
                                    session.find_above(
                                        prompt_bar::SEARCH_ABOVE,
                                        &prompt_bar::PROMPT_MARKS,
                                        |row, shaded| matches(row, shaded),
                                    )
                                })
                            })
                            .map(|i| history[i].as_str())
                    });
                    match pinned {
                        Some(prompt) => {
                            let row = session.size().cell_height as f32 / ctx.pixels_per_point();
                            if prompt_bar::show(
                                &ctx,
                                &self.chrome,
                                response.rect,
                                row,
                                self.shared.font_size,
                                active,
                                prompt,
                            ) {
                                self.focus_terminal = true;
                            }
                        }
                        None => prompt_bar::hide(&ctx, active),
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("no session");
                    });
                }
            }
        });
        if let Some((slot, painter)) = backdrop {
            let outer = panel.response.rect;
            let shape = if solid {
                egui::Shape::rect_filled(outer, 0.0, self.chrome.panel)
            } else {
                let fill = giverny_term::render::opacity::see_through(self.chrome.panel, opacity);
                egui::Shape::Vec(
                    around(outer, grid_rect)
                        .into_iter()
                        .map(|r| egui::Shape::rect_filled(r, 0.0, fill))
                        .collect(),
                )
            };
            painter.with_clip_rect(outer).set(slot, shape);
        }

        actions.extend(overlays::palette_ui(self, &ctx));
        actions.extend(overlays::sessions_ui(self, &ctx));
        actions.extend(keymap::overlay_ui(self, &ctx));

        for action in actions {
            self.apply(&ctx, action);
        }

        // A Ctrl+Tab walk ends the way Alt+Tab does: when Ctrl comes up, or
        // when the window stops being the one receiving keys at all.
        if self.switcher.is_some() {
            let holding = ctx.input(|i| i.modifiers.ctrl && i.focused);
            if holding {
                // Modifier releases arrive as events on every platform we
                // support, but a missed one would strand the walk open.
                ctx.request_repaint_after(Duration::from_millis(250));
            } else {
                self.end_switch();
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Reached on the way out of the event loop whether the window was
        // closed or the compositor died under it, so `closing` is what tells
        // the two apart in the state file.
        self.persist_all();
        for (_, rt) in self.rt.drain() {
            if let Some(session) = rt.session {
                session.shutdown();
            }
        }
    }
}

/// Is a Giverny already running?
///
/// Half of what `doctor` reports means something different depending on the
/// answer — "0 tabs reported by the distribution" is a broken sweep when the
/// app is up and the plainest fact in the world when it is not — and every
/// report so far has had to be read without knowing which.
fn app_is_running() -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let me = std::process::id();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys.processes().iter().any(|(pid, proc)| {
        pid.as_u32() != me
            && proc
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("giverny")
    })
}

/// What each saved tab would do when it is next opened.
///
/// The same plan the app follows, run against the workspace on disk. "Why did
/// this tab not come back" is otherwise a question only the app can answer,
/// and only while it is running.
fn restore_report(profs: &[giverny_claude::profiles::Profile]) {
    let paths = Paths::default_dirs();
    let Some(saved) = state::load(&paths) else {
        println!("\nrestore: no saved workspace yet");
        return;
    };
    let all_dirs: Vec<PathBuf> = profs.iter().map(|p| p.config_dir.clone()).collect();
    let tabs = &saved.workspace.tabs;
    println!("\nrestore ({} tab(s) in the saved workspace):", tabs.len());
    let mut resumable = 0;
    for tab in tabs {
        let title = tab.title();
        let Some(sid) = &tab.claude_session else {
            println!("  {title}\n    conversation  none recorded");
            continue;
        };
        println!("  {title}\n    conversation  {sid}");
        match resume_plan(
            sid,
            tab.cwd.as_deref(),
            tab.claude_config_dir.as_deref(),
            &all_dirs,
        ) {
            ResumePlan::Ready {
                account,
                dir,
                command,
            } => {
                resumable += 1;
                println!("    account       {}", account.display());
                println!(
                    "    directory     {}",
                    dir.map(|d| d.display().to_string())
                        .unwrap_or_else(|| "(unknown — resumes wherever the shell opens)".into())
                );
                println!("    would type    {}", command.trim_end());
            }
            ResumePlan::Malformed => println!("    SKIPPED       not a session id"),
            ResumePlan::LiveElsewhere => {
                println!("    SKIPPED       something is already running this conversation")
            }
            ResumePlan::NoTranscript => println!(
                "    SKIPPED       no transcript in any account — claude --resume would fail"
            ),
        }
    }
    if tabs.iter().all(|t| t.claude_session.is_none()) && !tabs.is_empty() {
        println!(
            "  nothing to resume: no tab has a conversation recorded.\n               ids are captured by the SessionStart hook, so a session that\n               started before hooks were installed is not one of them — run\n               claude once in a tab and check here again."
        );
    } else {
        println!("  {resumable} of {} would resume", tabs.len());
    }
}

/// `giverny doctor` — print exactly what the app sees of your Claude setup.
fn doctor() {
    use giverny_claude::{hooks, profiles, registry, usage};

    println!("giverny doctor {}\n", update::CURRENT);
    let running = app_is_running();
    println!(
        "app          {}\n",
        if running {
            "running (tabs and sessions below are live)"
        } else {
            "not running — nothing here has tabs, and \"0 tabs reported\" means only that"
        }
    );

    // Hook transport differs by platform: a unix socket where one exists,
    // otherwise the spool file the relay always falls back to.
    #[cfg(unix)]
    {
        let socket = hooks::socket_path();
        let listening = std::os::unix::net::UnixStream::connect(&socket).is_ok();
        println!(
            "relay        unix socket {} — {}\n",
            socket.display(),
            if listening {
                "listening (hooks deliver live)"
            } else {
                "nobody listening (events spool to disk)"
            }
        );
    }
    #[cfg(not(unix))]
    {
        println!(
            "relay        spool file {}\n",
            Paths::default_dirs().hook_spool().display()
        );
    }

    // On Wayland the taskbar icon comes from the desktop entry, not from us.
    #[cfg(target_os = "linux")]
    if let Some((entry, installed)) = desktop::status() {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        println!(
            "desktop entry {}",
            match (installed, wayland) {
                (true, _) => format!("installed ✓  {}", entry.display()),
                (false, true) => "MISSING — run `giverny install-desktop` \
                                  (Wayland takes the taskbar icon from it)"
                    .to_string(),
                (false, false) => "not installed (X11 uses the built-in window icon)".to_string(),
            }
        );
        println!();
    }

    // What systemd does to the rest of the scope when one tab's process is
    // OOM-killed. `stop` is the default, and it closes the whole terminal.
    #[cfg(target_os = "linux")]
    {
        let oom = oom::status();
        match oom.policy.as_deref() {
            Some("continue") => {
                println!("oom policy   continue ✓ (a killed child stays a killed child)")
            }
            Some(other) => println!(
                "oom policy   {other} — one OOM-killed process in any tab stops every tab;\n             run `giverny install-desktop`, then restart Giverny"
            ),
            None => println!(
                "oom policy   unknown (no systemd scope around this process)\n             drop-in {}",
                if oom.installed {
                    "installed ✓"
                } else {
                    "not installed"
                }
            ),
        }
        println!();
    }

    let cfg = config::load(Paths::default_dirs().base());
    // Usage numbers are refreshed by running `claude -p /usage`, so a
    // Giverny that cannot find claude shows whatever the cache last held and
    // says nothing about why.
    match usage::cli_path() {
        Some(path) => println!("claude       {}", path.display()),
        None => println!("claude       not found on PATH (on Windows, see wsl below)"),
    }
    // Where most Windows machines actually keep Claude Code.
    let distros = wsl::distros();
    if !distros.is_empty() {
        let default = wsl::default_distro();
        for distro in &distros {
            let mark = if Some(distro) == default.as_ref() {
                " (default)"
            } else {
                ""
            };
            println!("wsl          {distro}{mark}");
            match wsl::claude_bin(distro) {
                Some(bin) => println!("             claude  {bin}"),
                None => {
                    println!("             claude  NOT FOUND — usage cannot refresh for this one")
                }
            }
            match wsl::account_dir(distro) {
                Some(dir) if dir.is_dir() => {
                    println!("             account {}", dir.display());
                    // Naming a default account in CLAUDE_CONFIG_DIR moves
                    // where Claude Code looks for its login, so a session
                    // handed its own account comes up at the first-run
                    // prompt with no conversation to resume.
                    println!(
                        "             named   {}",
                        match profiles::must_be_named(&dir) {
                            true => "yes — CLAUDE_CONFIG_DIR goes in front of claude here",
                            false => "no — this is the distribution's default account",
                        }
                    );
                }
                Some(dir) => println!("             account none yet ({})", dir.display()),
                None => println!("             account unknown (no home reported)"),
            }
            // What the rail knows about where tabs are. Zero while Giverny
            // is closed is expected; zero while it is open and has tabs in
            // this distribution is the sweep failing.
            let found = wsl::tab_cwds_in(distro);
            println!(
                "             tabs    {} reported by the distribution{}",
                found.len(),
                match (found.is_empty(), running) {
                    (true, true) => " — GIVERNY TABS ARE OPEN, SO THIS IS THE SWEEP FAILING",
                    (true, false) => " (expected: giverny is not running)",
                    (false, _) => "",
                }
            );
            for (tab, cwd) in found {
                println!("                     {tab} {cwd}");
            }
        }
    }
    println!();

    let profs = profiles::discover(&cfg.behavior.extra_profile_dirs);
    if profs.is_empty() {
        println!("NO CLAUDE PROFILES FOUND — is Claude Code installed?");
        return;
    }
    // Name the sources consulted: when an account goes missing, "where do
    // these come from" is the actual question.
    let mut sources = vec!["~/.claude"];
    if giverny_claude::profiles::ambient_dirs().len() > 1 {
        sources.push("claude* in ~ and ~/.config");
    }
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        sources.push("$CLAUDE_CONFIG_DIR");
    }
    if std::env::var_os("CCTOP_CONFIG_DIRS").is_some() {
        sources.push("$CCTOP_CONFIG_DIRS");
    }
    if !cfg.behavior.extra_profile_dirs.is_empty() {
        sources.push("config");
    }
    println!(
        "profiles ({} found via {}):",
        profs.len(),
        sources.join(" + ")
    );
    drag_drop_status();
    println!("      an account kept elsewhere? add its directory in settings → claude,");
    println!("      or as behavior.extra_profile_dirs in config.toml");
    let now = jiff::Timestamp::now();
    for p in &profs {
        let settings = p.config_dir.join("settings.json");
        let hooks_ok = hooks::installed_in(&settings);
        let sl_ok = hooks::statusline_installed_in(&settings);
        println!(
            "\n  @{}  {}",
            p.name,
            p.email.as_deref().unwrap_or("(identity unknown)")
        );
        println!("    dir        {}", p.config_dir.display());
        println!(
            "    hooks      {}",
            if hooks_ok {
                "installed ✓"
            } else {
                "MISSING — click 'install' in the rail"
            }
        );
        // The command itself, because a hook pointing at a path its own shell
        // cannot run is installed, correct-looking, and completely inert.
        if let Some(cmd) = hooks::installed_command(&settings) {
            let runnable = !giverny_claude::wsl::is_wsl_path(&p.config_dir)
                || cmd.starts_with('/')
                || cmd.starts_with('\'');
            println!(
                "    runs       {cmd}{}",
                if runnable {
                    ""
                } else {
                    "  ← THIS IS A WINDOWS PATH; THE SHELL RUNNING IT IS NOT"
                }
            );
        }
        println!(
            "    statusline {}",
            if sl_ok {
                "installed ✓ (live usage)"
            } else {
                "not installed (usage from cache only)"
            }
        );
        match usage::read(&p.config_dir) {
            Some(u) => {
                let age = usage::age_minutes(&u, now);
                let buckets: Vec<String> = u
                    .limits
                    .iter()
                    .map(|l| format!("{} {}%", l.label(), l.effective_percent(now).round()))
                    .collect();
                println!("    usage      {} (cache {age}m old)", buckets.join(", "));
            }
            None => println!("    usage      no cache yet — run /usage once in this account"),
        }
        // What Giverny runs to move those numbers. Everything about it is
        // inferred — where claude is, whether the account is named, which
        // side of the WSL boundary it is on — so when the cache stops ageing
        // backwards this is the line to try by hand.
        match usage::refresh_describe(&p.config_dir) {
            Ok(cmd) => println!("    refresh    {cmd}"),
            Err(e) => println!("    refresh    CANNOT — {e}"),
        }
        println!(
            "    writes to  {}",
            profiles::identity_path(&p.config_dir).display()
        );
    }

    restore_report(&profs);

    let dirs: Vec<PathBuf> = profs.iter().map(|p| p.config_dir.clone()).collect();
    let live = registry::scan(dirs);
    println!("\nlive claude sessions ({}):", live.len());
    for s in &live {
        println!(
            "  pid {:<8} {:<6} {:<26} {}",
            s.entry.pid,
            s.entry.status,
            s.entry.name.as_deref().unwrap_or("-"),
            s.entry.cwd.display()
        );
    }

    println!(
        "\nnotes\n  · notifications fire when claude needs YOU (permission prompts, questions),\n    \
         not when it merely finishes"
    );
}

/// `giverny update` — headless check, for scripts and impatient users.
fn update_cli() {
    let paths = Paths::default_dirs();
    println!("giverny {CURRENT}", CURRENT = update::CURRENT);
    match update::fetch_latest() {
        Ok(latest) => match update::is_newer(&latest, update::CURRENT) {
            Some(true) => {
                println!(
                    "update available: {latest}\n\nrun:\n  {}",
                    update::install_command()
                );
            }
            _ => println!("up to date (latest release is {latest})"),
        },
        Err(err) => println!("could not reach github: {err}"),
    }
    let _ = paths;
}

fn config_mtime(paths: &Paths) -> Option<std::time::SystemTime> {
    std::fs::metadata(config::config_path(paths.base()))
        .ok()
        .and_then(|m| m.modified().ok())
}

fn desktop_notify(summary: String, body: String) {
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("Giverny")
            .summary(&summary)
            .body(&body)
            .show();
    });
}

fn fresh_nonce(salt: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}{:x}", nanos, std::process::id(), salt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare word the binary does not know is a command, and is refused;
    /// flags and anything that reads as a path still open the window.
    #[test]
    fn unknown_words_are_commands_but_paths_and_flags_are_not() {
        for word in ["doctr", "upgrade", "plan", "help", "status-line"] {
            assert!(is_unknown_subcommand(word, false), "{word}");
        }
        for not_a_command in [
            "",
            "-psn_0_12345",
            "--some-flag",
            "./here",
            "..",
            "~/Dev",
            "/home/me/project",
            "proj/sub",
            "C:\\Users\\me",
        ] {
            assert!(
                !is_unknown_subcommand(not_a_command, false),
                "{not_a_command}"
            );
        }
        // A bare word naming something that exists is a path, not a command.
        assert!(!is_unknown_subcommand("project", true));
    }

    /// One bad value elsewhere still lets account setup follow the config; a
    /// bad value in what it reads does not.
    #[test]
    fn account_setup_follows_a_config_unless_its_own_values_are_bad() {
        let parse = |text| config::parse_over(text, &config::Config::default()).unwrap();
        assert!(accounts_readable(&parse("[font]\nsize = \"big\"\n")));
        assert!(accounts_readable(&parse("[claude]\nauto_mode = false\n")));
        assert!(!accounts_readable(&parse("[claude]\nauto_mode = \"no\"\n")));
        assert!(!accounts_readable(&parse("claude = 5\n")));
    }

    /// A config that cannot be parsed leaves every account as it was —
    /// here one auto mode would otherwise be written into — while a parsed
    /// one is followed.
    #[test]
    fn an_unparseable_config_leaves_the_accounts_alone() {
        let root = std::env::temp_dir().join(format!(
            "giverny-unread-config-{}-{}",
            std::process::id(),
            jiff::Timestamp::now().as_nanosecond()
        ));
        let account = root.join("claude");
        std::fs::create_dir_all(&account).unwrap();
        let settings = account.join("settings.json");
        let before = r#"{"theme":"dark"}"#;
        std::fs::write(&settings, before).unwrap();

        let mut watch = claude_watch::ClaudeWatch::for_tests();
        watch.profiles = vec![giverny_claude::profiles::Profile {
            name: "test".into(),
            config_dir: account.clone(),
            email: None,
            account_uuid: None,
        }];
        let mut auto = config::Config::default();
        auto.claude.auto_mode = true;

        set_up_accounts(&mut watch, &auto, false);
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);

        // The same settings from a config that was read: the account follows.
        set_up_accounts(&mut watch, &auto, true);
        let after = std::fs::read_to_string(&settings).unwrap();
        assert!(after.contains("defaultMode"), "{after}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A press on anything else — here a button, as the rail
    /// would be — takes egui's focus off the terminal, and
    /// the terminal is then told to take it back; a text field keeps it.
    #[test]
    fn the_terminal_takes_the_keyboard_back_from_all_but_text_fields() {
        let ctx = egui::Context::default();
        let button_at = egui::pos2(20.0, 10.0);
        let mut text = String::new();
        let frame = |events: Vec<egui::Event>, with_field: bool, text: &mut String| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut lost = false;
            let _ = ctx.run_ui(input, |ui| {
                let _ = ui.button("row");
                if with_field {
                    ui.add(egui::TextEdit::singleline(text)).request_focus();
                }
                let (_, resp) =
                    ui.allocate_exact_size(egui::vec2(400.0, 300.0), egui::Sense::click_and_drag());
                lost = terminal_lost_keys(ui.ctx(), resp.id, false);
                if lost {
                    resp.request_focus();
                }
            });
            lost
        };
        let press = |pressed| egui::Event::PointerButton {
            pos: button_at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // Unfocused at first: taken.
        assert!(frame(vec![], false, &mut text));
        assert!(!frame(vec![], false, &mut text));
        // A click on the button: lost, and taken back.
        let lost = frame(
            vec![egui::Event::PointerMoved(button_at), press(true)],
            false,
            &mut text,
        ) | frame(vec![press(false)], false, &mut text);
        assert!(lost);
        assert!(!frame(vec![], false, &mut text));
        // A text field holding the keyboard keeps it.
        frame(vec![], true, &mut text);
        assert!(!frame(vec![], true, &mut text));
        assert!(ctx.text_edit_focused());
        // An overlay reading its own keys: left alone.
        assert!(!terminal_lost_keys(&ctx, egui::Id::new("term"), true));
    }

    #[test]
    fn the_backdrop_tiles_around_the_grid_exactly() {
        let outer = egui::Rect::from_min_max(egui::pos2(240.0, 0.0), egui::pos2(1280.0, 820.0));
        let grid = egui::Rect::from_min_max(egui::pos2(248.0, 14.0), egui::pos2(1272.0, 812.0));
        let parts = around(outer, Some(grid));
        assert_eq!(parts.len(), 4);
        let area: f32 = parts.iter().map(|r| r.area()).sum();
        assert_eq!(area, outer.area() - grid.area(), "gaps or overlaps");
        for (i, a) in parts.iter().enumerate() {
            assert!(!a.intersects(grid.shrink(0.01)), "part {i} covers the grid");
            for b in &parts[i + 1..] {
                assert!(!a.shrink(0.01).intersects(b.shrink(0.01)), "parts overlap");
            }
        }
        assert_eq!(around(outer, None), vec![outer]);
    }

    #[test]
    fn software_gl_renderers_are_told_from_gpus() {
        assert!(is_software_gl("llvmpipe (LLVM 20.1.2, 256 bits)"));
        assert!(is_software_gl("GDI Generic"));
        assert!(!is_software_gl("D3D12 (Intel(R) Graphics)"));
        assert!(!is_software_gl("Mesa Intel(R) Graphics (MTL)"));
    }

    #[test]
    fn windows_draws_on_dx12_unless_wgpu_backend_says_otherwise() {
        assert_eq!(
            preferred_backends_for(true, false),
            Some(eframe::wgpu::Backends::DX12)
        );
        assert_eq!(preferred_backends_for(true, true), None);
        assert_eq!(preferred_backends_for(false, false), None);
    }

    #[test]
    fn a_software_adapter_opens_on_opengl() {
        use eframe::wgpu::DeviceType;
        let adapter = |device_type| eframe::wgpu::AdapterInfo {
            name: "llvmpipe (LLVM 19.1.1, 256 bits)".into(),
            vendor: 0x10005,
            device: 0,
            device_type,
            device_pci_bus_id: String::new(),
            driver: "llvmpipe".into(),
            driver_info: "Mesa 25.0.7".into(),
            backend: eframe::wgpu::Backend::Vulkan,
            subgroup_min_size: 8,
            subgroup_max_size: 8,
            transient_saves_memory: false,
        };
        assert_eq!(renderer_for(None), eframe::Renderer::Glow);
        assert_eq!(
            renderer_for(Some(&adapter(DeviceType::Cpu))),
            eframe::Renderer::Glow
        );
        for gpu in [
            DeviceType::DiscreteGpu,
            DeviceType::IntegratedGpu,
            DeviceType::VirtualGpu,
            DeviceType::Other,
        ] {
            assert_eq!(
                renderer_for(Some(&adapter(gpu))),
                eframe::Renderer::Wgpu,
                "{gpu:?}"
            );
        }
    }

    #[test]
    fn resume_thresholds_are_set_only_when_asked() {
        let mut claude = config::ClaudeConfig::default();
        assert!(claude_env(&claude).is_empty(), "nothing by default");

        claude.skip_resume_summary = true;
        let env = claude_env(&claude);
        let value = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.parse::<u64>().expect("a number"))
                .unwrap_or_else(|| panic!("{key} missing"))
        };
        // Claude Code's own defaults are 70 minutes and 100k tokens; the
        // prompt appears only above both, so both have to be out of reach.
        assert!(value("CLAUDE_CODE_RESUME_THRESHOLD_MINUTES") > 70);
        assert!(value("CLAUDE_CODE_RESUME_TOKEN_THRESHOLD") > 100_000);
    }

    /// What ita saw: every resumed conversation in his distribution came up
    /// at Claude Code's first-run prompt instead. The command carried
    /// `CLAUDE_CONFIG_DIR=/home/ita/.claude` — his own default account —
    /// which moves where Claude Code looks for its identity, so it found
    /// none, and a Claude that has just met you knows no conversation to
    /// resume. The account is named only when it is a profile.
    #[test]
    fn a_default_account_is_not_named_in_the_resume_command() {
        let root = std::env::temp_dir().join(format!("giverny-resume-{}", std::process::id()));
        let sid = "4eb78e38-0f2e-4a7d-a58c-045e90c5b912";
        let plan_for = |account: &Path, cwd: &str| {
            let project = account.join("projects").join("-home-ita-Inbar");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(
                project.join(format!("{sid}.jsonl")),
                format!("{{\"cwd\":\"{cwd}\"}}\n"),
            )
            .unwrap();
            resume_plan(sid, Some(account), None, &[account.to_path_buf()])
        };

        let home = root.join("home").join("ita");
        let default = home.join(".claude");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(default.with_extension("json"), r#"{"oauthAccount":{}}"#).unwrap();
        match plan_for(&default, "/home/ita/Inbar") {
            ResumePlan::Ready { command, .. } => {
                assert!(!command.contains("CLAUDE_CONFIG_DIR"), "{command}");
                assert!(command.starts_with("cd \"/home/ita/Inbar\" && command claude"));
            }
            other => panic!("{other:?}"),
        }

        // A profile is the other way round: it is only reachable by name.
        let work = home.join(".claude-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(".claude.json"), r#"{"oauthAccount":{}}"#).unwrap();
        match plan_for(&work, "/home/ita/Inbar") {
            ResumePlan::Ready { command, .. } => assert!(
                command.contains(&format!("CLAUDE_CONFIG_DIR=\"{}\"", work.display())),
                "{command}"
            ),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ita's complaint: an agent working in one project opened a tab in
    /// whichever category he happened to be looking at.
    #[test]
    fn an_agent_is_filed_with_its_own_work() {
        let mut ws = Workspace::default();
        let other = ws.add_category("notes");
        let hoteleak = ws.add_category("hoteleak");
        let tab = ws.add_tab(hoteleak);
        ws.tab_mut(tab).unwrap().cwd = Some(PathBuf::from("/home/ita/Dev/hoteleak"));
        // Adding a tab focuses it, so this is the category on screen.
        ws.add_tab(other);

        // A category already working in that directory takes it, whatever is
        // on screen at the time.
        assert_eq!(
            category_for_agent(&mut ws, Some(Path::new("/home/ita/Dev/hoteleak"))),
            hoteleak
        );
        // Nowhere to put it: a category named after the directory, made once
        // and reused after that.
        let made = category_for_agent(&mut ws, Some(Path::new("/home/ita/Dev/giverny")));
        assert_eq!(
            ws.categories.iter().find(|c| c.id == made).unwrap().name,
            "giverny"
        );
        assert_eq!(
            category_for_agent(&mut ws, Some(Path::new("/home/ita/Dev/giverny"))),
            made,
            "a second agent joins the first"
        );
        // No directory at all falls back to where the user is.
        assert_eq!(category_for_agent(&mut ws, None), other);
    }

    /// The regression that shipped in v0.5.3: every Windows tab opened
    /// `wsl.exe -d <distro> ~`, and wsl.exe reads that `~` as a command.
    #[test]
    fn a_wsl_tab_opens_in_the_home_directory() {
        assert_eq!(
            wsl_shell("Ubuntu", Some("Ubuntu"), None),
            ("wsl.exe".into(), vec!["~".to_string()]),
            "the default distribution takes the shorthand that works"
        );
        assert_eq!(
            wsl_shell("Debian", Some("Ubuntu"), None),
            (
                "wsl.exe".into(),
                vec![
                    "-d".to_string(),
                    "Debian".to_string(),
                    "--cd".to_string(),
                    "~".to_string()
                ]
            ),
            "naming one needs the flag, not the shorthand"
        );
        // Arguments are joined unquoted, so a name with a space is quoted here.
        let (_, args) = wsl_shell("Ubuntu 22.04", Some("Ubuntu"), None);
        assert_eq!(args[1], "\"Ubuntu 22.04\"");
    }

    /// Reopening where the tab was, which is the whole point of remembering.
    #[test]
    fn a_wsl_tab_reopens_where_it_was() {
        assert_eq!(
            wsl_shell("Ubuntu", Some("Ubuntu"), Some("/home/ita/proj")),
            (
                "wsl.exe".into(),
                vec!["--cd".to_string(), "/home/ita/proj".to_string()]
            )
        );
        let (_, args) = wsl_shell("Ubuntu", Some("Ubuntu"), Some("/home/ita/my proj"));
        assert_eq!(args[1], "\"/home/ita/my proj\"");

        // A Windows path is not somewhere a distribution can be sent, and a
        // tab with nowhere remembered goes home.
        assert_eq!(
            wsl_start_dir("Ubuntu", Some(Path::new(r"C:\Users\ita"))),
            None
        );
        assert_eq!(wsl_start_dir("Ubuntu", None), None);
    }

    /// The variables a hook inside WSL needs, without dropping the ones the
    /// user already shares — `%WSLENV%` is a machine-wide setting, and
    /// replacing it would quietly break whatever else relies on it.
    #[test]
    fn wslenv_adds_ours_and_keeps_theirs() {
        let mine = wslenv(None, &["GIVERNY_TAB_ID", "GIVERNY_NONCE"]);
        assert_eq!(mine, "GIVERNY_TAB_ID:GIVERNY_NONCE");

        let merged = wslenv(Some("EDITOR:PROJECT/p".into()), &["GIVERNY_TAB_ID"]);
        assert!(
            merged.starts_with("EDITOR:PROJECT/p:"),
            "theirs first: {merged}"
        );
        assert!(merged.contains("GIVERNY_TAB_ID"));

        // A variable they already share is not listed twice, flag and all.
        let dedup = wslenv(Some("CLAUDE_CONFIG_DIR/p".into()), &["CLAUDE_CONFIG_DIR"]);
        assert_eq!(dedup.matches("CLAUDE_CONFIG_DIR").count(), 1, "{dedup}");
        assert!(
            dedup.contains("CLAUDE_CONFIG_DIR/p"),
            "their flag survives: {dedup}"
        );

        // Windows Terminal exports a trailing colon; an empty entry there
        // would list a variable with no name.
        let theirs = wslenv(Some("WT_SESSION:WT_PROFILE_ID:".into()), &["GIVERNY_NONCE"]);
        assert_eq!(theirs, "WT_SESSION:WT_PROFILE_ID:GIVERNY_NONCE");
        assert!(!theirs.contains("::"), "{theirs}");
    }
}
