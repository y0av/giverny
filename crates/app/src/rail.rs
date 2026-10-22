//! The left rail: categories and tabs.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, TextEdit, Ui, Vec2,
};
use std::path::{Path, PathBuf};

use giverny_core::state::RailView;
use giverny_core::tabs::{CategoryId, TabId};
use giverny_term::render::bidi::egui_line;

use crate::claude_watch::{ClaudeState, ClaudeWatch, Freshness};
use crate::{Action, App, RenameTarget};
use giverny_claude::use_reading::Use;

const ROW_H: f32 = 40.0;
const HEADER_H: f32 = 26.0;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// One step of the rail's glyph animations (spinner, pulse), in seconds.
///
/// Every step is a full-window repaint, and on a software renderer (WSLg's
/// llvmpipe) a repaint is real CPU: the old per-frame 120 ms cadence cost
/// ~95% of a core for one spinner (#43). Four steps a second still reads as
/// moving; half that while the window is in the background. The clock is
/// the terminal's output pacing clock, so a spinner and a working tab on
/// screen share their frames.
const ANIM_STEP: f64 = giverny_term::pace::STEP_FOCUSED.as_secs_f64();

/// The animation clock, snapped to whole steps: every glyph drawn in one
/// frame is on the same step, and a repaint that happens for some other
/// reason (terminal output, the pointer) does not nudge them in between.
fn anim_time() -> f64 {
    giverny_term::pace::anim_time()
}

/// Whether the rail draws its moving marks (`rail.animate`). Set once
/// a frame by [`show`]. Off, no spinner, ring or pulse is drawn at all (their
/// place stays blank, so nothing beside them moves), and nothing here asks
/// for a frame.
fn animating(ui: &Ui) -> bool {
    ui.ctx()
        .data(|d| d.get_temp(egui::Id::new(ANIMATE_ID)))
        .unwrap_or(true)
}

const ANIMATE_ID: &str = "rail-animate";

/// Keep an animation drawn at `rect` moving: wake for the clock's next tick,
/// but only while it is on screen. A row scrolled out of the rail, or a
/// spinner nobody can see, costs nothing.
fn keep_animating(ui: &Ui, rect: Rect) {
    if !animating(ui) || !ui.is_rect_visible(rect) {
        return;
    }
    let (focused, predicted_dt) = ui.input(|i| (i.focused, i.predicted_dt));
    ui.ctx()
        .request_repaint_after(giverny_term::pace::until_next_tick(focused, predicted_dt));
}

/// Where the rail's spinners point, in turns, and a request for their next
/// frame while `rect` is on screen.
///
/// On a GPU they turn once a second on the real clock, twelve frames a
/// second ([`giverny_term::pace::SPIN_PER_STEP`]): evenly, because the angle is where
/// the clock is when the frame is drawn, not which snapped step it fell in
/// (#70). On a software renderer every frame is CPU, so they keep to the
/// shared four-a-second clock at half a turn a second, eight positions to a
/// turn (#43).
///
/// `None` with `rail.animate` off: then there is no spinner to draw.
fn spin(ui: &Ui, rect: Rect) -> Option<f64> {
    if !animating(ui) {
        return None;
    }
    keep_spinning(ui, rect);
    Some(spin_turns(ui))
}

/// [`spin`]'s angle alone, for a spinner whose rect is not known yet.
fn spin_turns(ui: &Ui) -> f64 {
    if giverny_term::pace::cheap_frames() {
        ui.input(|i| i.time)
    } else {
        anim_time() / 2.0
    }
}

/// [`spin`]'s next frame alone.
fn keep_spinning(ui: &Ui, rect: Rect) {
    if !giverny_term::pace::cheap_frames() {
        return keep_animating(ui, rect);
    }
    if !animating(ui) || !ui.is_rect_visible(rect) {
        return;
    }
    let (focused, predicted_dt) = ui.input(|i| (i.focused, i.predicted_dt));
    ui.ctx()
        .request_repaint_after(giverny_term::pace::until_next_spin(focused, predicted_dt));
}

/// The braille spinner's glyph at `turns` (see [`spin_turns`]): one lap of
/// its ten a turn — ten a second on a GPU, one a step on the CPU clock.
/// One breath every two seconds, from 0.3 to 1, sampled at the animation
/// step: how something that is waiting on the user asks for them.
fn breath(time: f64) -> f32 {
    ((time * std::f64::consts::PI).sin() * 0.35 + 0.65).clamp(0.0, 1.0) as f32
}

/// How a working tab is marked: the spinner, or with `rail.calm_busy` a dot
/// that breathes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyMark {
    Spinner,
    Breath,
}

impl BusyMark {
    fn of(cfg: &giverny_core::config::RailConfig) -> Self {
        if cfg.calm_busy {
            BusyMark::Breath
        } else {
            BusyMark::Spinner
        }
    }
}

/// One calm breath, in seconds.
const CALM_PERIOD: f64 = 3.0;

/// How far in the calm mark's breath is, 0 out to 1 in: one slow breath
/// every three seconds, against the amber flag's two (see [`breath`]), so
/// "working" and "wants you" never fall into step. `None` is a rail that
/// does not move: the dot holds still, full.
fn calm_breath(time: Option<f64>) -> f32 {
    match time {
        Some(t) => (0.5 - 0.5 * (t * std::f64::consts::TAU / CALM_PERIOD).cos()) as f32,
        None => 1.0,
    }
}

/// A working tab's mark at `at`, kept moving while `rect` is on screen. With
/// `rail.animate` off the spinner is left out and the calm mark holds still,
/// breathed out to the whole ring.
fn busy(ui: &Ui, p: &egui::Painter, rect: Rect, at: Pos2, color: Color32, mark: BusyMark) {
    match mark {
        BusyMark::Spinner => {
            if let Some(turns) = spin(ui, rect) {
                spinner(p, at, turns, color);
            }
        }
        BusyMark::Breath => {
            let time = animating(ui).then(anim_time);
            if time.is_some() {
                keep_animating(ui, rect);
            }
            calm_ring(p, at, calm_breath(time), color);
        }
    }
}

/// The open ring's radius: what Claude's calm busy mark breathes out to
/// from the idle dot ([`DOT_R`]), and holds at with `rail.animate` off.
/// The ring's inner edge: its stroke ([`RING_STROKE`]) lies outside it, as
/// egui paints a circle's stroke.
const RING_R: f32 = 4.0;

/// The small filled dot's radius: Claude idle at its prompt in the
/// category's colour, and a restored Claude tab not started yet in gray.
/// A touch bigger than it was (2.5) so it reads as round beside the small
/// square at a glance, still well inside the ring.
const DOT_R: f32 = 3.0;

/// The open ring's stroke, from [`RING_R`] outwards.
const RING_STROKE: f32 = 1.4;

/// The hollow square a running command's square breathes out to, across,
/// and holds at with `rail.animate` off: about as big to the eye as
/// Claude's ring at the top of its breath (was 7.0, which looked small
/// beside it). A square reads larger than a circle as wide, so it stays
/// narrower than the ring's outer edge.
const SQUARE_SIDE: f32 = 9.5;

/// The small filled square, across: a shell at its prompt in the
/// category's colour, as idle Claude is the small dot; a restored shell tab
/// not started yet, in gray; and the size a running command's square
/// breathes in to. Was 4.0; a little bigger so it reads as square beside
/// the dot ([`DOT_R`]), still well inside [`SQUARE_SIDE`].
const SMALL_SQUARE: f32 = 5.0;

/// How many sides [`annulus`] gives its circles: rounder than the 16 egui
/// gives a circle this small.
const ANNULUS_SIDES: u32 = 32;

/// A disc at `at` with a hole in its middle: everything between `inner` and
/// `outer` from the centre is `color`, drawn as one mesh, so as the hole
/// closes the band simply thickens inwards until it fills the disc. Both
/// edges are anti-aliased across one device pixel, as egui's own; a hole
/// narrower than that does not vanish in a step but fades out in the
/// centre (see [`annulus_ink`]). One shape at every size: never a second
/// one filling the hole in, which showed as a dot inside the ring just
/// before it closed.
fn annulus(p: &egui::Painter, at: Pos2, outer: f32, inner: f32, color: Color32) {
    let px = 1.0 / p.pixels_per_point();
    let outer = outer.max(0.0);
    let inner = inner.clamp(0.0, outer);
    if outer <= 0.0 {
        return;
    }
    // The radii where the ink's slope changes, innermost first: between
    // two of them it is a straight ramp, which the mesh's colours draw
    // exactly.
    let mut radii = [
        inner - px / 2.0,
        inner + px / 2.0,
        outer - px / 2.0,
        outer + px / 2.0,
    ]
    .map(|r| r.max(0.0))
    .to_vec();
    radii.sort_by(f32::total_cmp);
    radii.dedup();
    let ink = |r: f32| color.gamma_multiply(annulus_ink(r, outer, inner, px));
    let mut mesh = egui::Mesh::default();
    // A ring of vertices round the centre at each radius; at radius 0 the
    // centre alone. With the hole open the innermost ring has no ink and
    // nothing is drawn inside it.
    let mut rings = Vec::new();
    for &r in &radii {
        let first = mesh.vertices.len() as u32;
        if r <= 0.0 {
            mesh.colored_vertex(at, ink(0.0));
        } else {
            for i in 0..ANNULUS_SIDES {
                let turn = std::f32::consts::TAU * i as f32 / ANNULUS_SIDES as f32;
                mesh.colored_vertex(at + r * Vec2::angled(turn), ink(r));
            }
        }
        rings.push((first, r <= 0.0));
    }
    for pair in rings.windows(2) {
        let [(a, centre), (b, _)] = [pair[0], pair[1]];
        for i in 0..ANNULUS_SIDES {
            let j = (i + 1) % ANNULUS_SIDES;
            if centre {
                mesh.add_triangle(a, b + i, b + j);
            } else {
                mesh.add_triangle(a + i, b + i, b + j);
                mesh.add_triangle(a + i, b + j, a + j);
            }
        }
    }
    p.add(egui::Shape::mesh(mesh));
}

/// How much ink [`annulus`] lays `r` from its centre, 0 to 1: a ramp one
/// pixel (`px`) wide over each edge, as egui feathers its own. A hole
/// under a pixel across cannot be ramped so, and its centre would stay
/// half filled; its ramp instead starts in the very centre, at
/// `1 - 2 * inner / px`, so the centre clears by degrees as the hole opens
/// and fills by degrees as it closes, meeting the full ramp at half a pixel.
fn annulus_ink(r: f32, outer: f32, inner: f32, px: f32) -> f32 {
    let edge = (outer - r) / px + 0.5;
    let hole = if inner >= px / 2.0 {
        (r - inner) / px + 0.5
    } else {
        let centre = 1.0 - 2.0 * inner / px;
        centre + (1.0 - centre) * r / (inner + px / 2.0)
    };
    edge.min(hole).clamp(0.0, 1.0)
}

/// Claude's calm busy mark (`rail.calm_busy`): its idle dot breathing out to
/// the open ring and back. At `breath` 0 it is the idle dot ([`DOT_R`],
/// filled); as it breathes out its edge grows to the ring's and a hole opens
/// in its middle, until at 1 it is the hollow ring of [`RING_R`], as with
/// `rail.animate` off. It is one shape throughout, its edge and its hole
/// moving together, so it morphs and never jumps between filled and stroked.
fn calm_ring(p: &egui::Painter, at: Pos2, breath: f32, color: Color32) {
    let breath = breath.clamp(0.0, 1.0);
    let outer = DOT_R + (RING_R + RING_STROKE - DOT_R) * breath;
    let inner = RING_R * breath;
    annulus(p, at, outer, inner, color);
}

/// A shell running a command: its prompt's small filled square
/// ([`SMALL_SQUARE`]) breathing out to the hollow square ([`SQUARE_SIDE`])
/// and back, on the calm mark's slow breath, as Claude's busy dot breathes to its ring
/// ([`calm_ring`]). Its edge grows and a hole opens in its middle together,
/// so it morphs and never jumps between filled and hollow. Kept moving on
/// the animation clock while `rect` is on screen; with `rail.animate` off
/// it holds still as the hollow square.
fn breathing_square(ui: &Ui, p: &egui::Painter, rect: Rect, at: Pos2, color: Color32) {
    let time = animating(ui).then(anim_time);
    if time.is_some() {
        keep_animating(ui, rect);
    }
    square_breath(p, at, calm_breath(time), color);
}

/// [`breathing_square`]'s shape at `breath`: 0 is the small filled square
/// of the prompt, 1 the hollow one exactly as [`square`] draws it.
fn square_breath(p: &egui::Painter, at: Pos2, breath: f32, color: Color32) {
    let breath = breath.clamp(0.0, 1.0);
    let outer = SMALL_SQUARE + (SQUARE_SIDE - SMALL_SQUARE) * breath;
    let hole = (SQUARE_SIDE - 2.0 * square_stroke(p.pixels_per_point())) * breath;
    square_band(p, at, outer, hole, color);
}

/// How lit each of a background shell's three dots is, 0 (dim) to 1 (the
/// tab's colour), as a colour travels through them left to right: one lap
/// every 3.75 s, slower than the calm mark's 3 s breath and the amber flag's
/// 2 s, so "something hums along" never falls into step with "working" or
/// "wants you". Fifteen animation steps a lap, five from one dot's peak to
/// the next, so each peak lands on the clock's grid. Each dot's glow spans
/// two thirds of a lap: it fades up as the one before it fades down, and at
/// any moment at most two are lit. `None` is a rail that does not move: all
/// three dim and still, as they were before they moved.
fn shell_wave(time: Option<f64>) -> [f32; 3] {
    const LAP: f64 = 3.75;
    const HALF_WIDTH: f64 = 1.0 / 3.0;
    let Some(t) = time else {
        return [0.0; 3];
    };
    std::array::from_fn(|i| {
        let phase = (t / LAP - i as f64 / 3.0).rem_euclid(1.0);
        let off = phase.min(1.0 - phase);
        if off >= HALF_WIDTH {
            0.0
        } else {
            (0.5 + 0.5 * (std::f64::consts::PI * off / HALF_WIDTH).cos()) as f32
        }
    })
}

/// A Claude tab's background shell at `at`: three small dots, a colour
/// moving through them from a faded `color` up to the full one, kept moving
/// while `rect` is on screen. All in the tab's category colour, like every
/// other mark: the wave's motion says "humming", the colour
/// says whose. With `rail.animate` off they hold still and faded, and
/// nothing asks for a frame.
fn shell_dots(ui: &Ui, p: &egui::Painter, rect: Rect, at: Pos2, color: Color32) {
    let time = animating(ui).then(anim_time);
    if time.is_some() {
        keep_animating(ui, rect);
    }
    let faded = color.gamma_multiply(WAVE_REST);
    for (i, lit) in shell_wave(time).into_iter().enumerate() {
        p.circle_filled(
            at + Vec2::new((i as f32 - 1.0) * 4.0, 0.0),
            1.4,
            faded.lerp_to_gamma(color, lit),
        );
    }
}

/// How lit a wave dot is between peaks, as a share of the category colour:
/// faint enough that the moving peak stands out, enough that the three
/// still read as the tab's colour and not as gray.
const WAVE_REST: f32 = 0.35;

fn braille(turns: f64) -> String {
    let step = if giverny_term::pace::cheap_frames() {
        (turns * SPINNER.len() as f64) as usize
    } else {
        (turns * 2.0 / ANIM_STEP) as usize
    };
    SPINNER[step % SPINNER.len()].to_string()
}

// Chrome colours come from the active theme (see `chrome`), reached through
// `app.chrome`. Only geometry is constant here.

struct RowData {
    id: TabId,
    title: String,
    sub: String,
    active: bool,
    exited: bool,
    /// Its shell has been spawned. A tab restored from a previous run has
    /// none until it is first focused.
    started: bool,
    /// Started, it comes back as Claude: it recorded a conversation to
    /// resume, or a `claude --resume` it was last running (what
    /// `queue_resume` acts on). What tells a not-started Claude tab from a
    /// not-started shell.
    resumes_claude: bool,
    color: Color32,
    claude: ClaudeState,
    /// Something Claude started still works — a background shell, or
    /// subagents it handed work to — while Claude itself waits at its
    /// prompt, free to talk.
    background: bool,
    /// A plain shell with a command in front: seen by the foreground poll,
    /// or between the shell's OSC 133 `C` and `D`.
    running: bool,
    /// Something finished here while you were on another tab and you have
    /// not looked since: a Claude turn, or a shell's command.
    /// Outlives Claude's `DoneUnseen`, which a new turn replaces.
    unseen: bool,
    /// Its shell's CPU and memory now, with everything under it (Claude
    /// Code or not), from the reading the total line shows; `None` for a
    /// tab with no shell running.
    used: Option<Use>,
}

/// What a group of rows is: a category you made, or a repository the tabs
/// turned out to be in.
#[derive(Clone, PartialEq)]
enum GroupKey {
    Category(CategoryId),
    /// `None` is the group for tabs in no repository at all.
    Repo(Option<PathBuf>),
}

struct GroupData {
    key: GroupKey,
    /// The category a new tab from this header lands in. A repository is a
    /// place tabs *are*, not one they can be put in, so a repository group
    /// borrows the category of the tabs already there.
    category: CategoryId,
    /// Where a new tab from this header starts.
    cwd: Option<PathBuf>,
    name: String,
    color: Color32,
    collapsed: bool,
    count: usize,
    busy: usize,
    needs: usize,
    /// Its tabs' readings summed, drawn on its header while it is
    /// collapsed; `None` when none of them has one.
    used: Option<Use>,
    /// A tab in it shows the tick, alone or as a badge: something finished
    /// there while you were elsewhere.
    unseen: bool,
    rows: Vec<RowData>,
}

/// One tab's row.
///
/// `under` is the repository the row is being shown inside, whose name is
/// already on the header above it: the path then only has to say what the
/// header does not, which is where in the repository this tab is.
fn row_data(
    app: &App,
    t: &giverny_core::tabs::Tab,
    color: Color32,
    under: Option<&Path>,
) -> RowData {
    let relative = under.zip(t.cwd.as_deref()).and_then(|(repo, cwd)| {
        let rest = cwd.strip_prefix(repo).ok()?;
        Some(if rest.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}", rest.display())
        })
    });
    let mut sub = match relative {
        Some(rest) => rest,
        None => t
            .cwd
            .as_deref()
            .map(|p| giverny_core::short_path(p, 24))
            .unwrap_or_default(),
    };
    if let Some(branch) = &t.git_branch {
        sub = if sub.is_empty() {
            format!(" {branch}")
        } else {
            format!("{sub} ·  {branch}")
        };
    }
    // A tab waiting on a window says so, and when: stopped and stopped-until
    // look identical otherwise.
    if app.is_limited(t.id) {
        let until = match app.limited_until(t.id) {
            Some(at) => {
                let zoned = at.to_zoned(jiff::tz::TimeZone::system());
                format!("resumes {}", zoned.strftime("%H:%M"))
            }
            None => "waiting for the window".to_string(),
        };
        sub = if sub.is_empty() {
            format!("out of limit · {until}")
        } else {
            format!("{sub} · out of limit · {until}")
        };
    }
    let ct = app.claude.tabs.get(&t.id);
    if let Some(account) = ct.and_then(|c| c.account.as_deref()) {
        sub = if sub.is_empty() {
            format!("@{account}")
        } else {
            format!("{sub} · @{account}")
        };
    }
    if app.codex.state_of(t.id) != ClaudeState::None {
        sub = if sub.is_empty() {
            "Codex".into()
        } else {
            format!("{sub} · Codex")
        };
    }
    RowData {
        id: t.id,
        title: t.display_title(&app.cfg.titles),
        sub,
        active: app.ws.active == Some(t.id),
        exited: t.exited,
        started: app.rt.contains_key(&t.id),
        resumes_claude: t.claude_session.is_some()
            || t.agent_session.is_some()
            || t.foreground
                .as_deref()
                .and_then(giverny_core::procs::resume_session_of)
                .is_some(),
        color,
        claude: app.agent_state(t.id),
        background: ct.is_some_and(|c| c.working_in_background()),
        running: app.fg_running.get(&t.id) == Some(&true) || app.osc_running.contains(&t.id),
        unseen: app.unseen_done.contains(&t.id)
            || ct.is_some_and(|c| c.state == ClaudeState::DoneUnseen),
        // Off (`rail.tab_figures`), a row has no figures, so nothing
        // reserves room for them and no group sums them.
        used: app
            .rt
            .get(&t.id)
            .filter(|_| app.cfg.rail.tab_figures)
            .and_then(|rt| rt.session.as_ref()?.child_pid)
            .and_then(|shell| app.use_now.as_ref()?.tabs.get(&shell).copied()),
    }
}

/// Counts a header carries: how many, how many working, how many want you.
///
/// Working is every tab whose mark says something runs — the agent itself
/// ([`Mark::Busy`]), what it left humming at its prompt ([`Mark::Wave`]:
/// subagents, a background shell, seen or unseen), or a plain shell running a
/// command ([`Mark::Running`]). Counting only the busy state let the header's
/// breathing dot go still while a tab still showed the wave; it should stop
/// only once nothing in the group runs, collapsed or not. A tab not started
/// this run has nothing running, whatever state lingers for it, so it is not
/// counted.
fn tallies(rows: &[RowData]) -> (usize, usize, usize) {
    (
        rows.len(),
        rows.iter()
            .filter(|r| matches!(Mark::of(r), Mark::Busy | Mark::Wave | Mark::Running))
            .count(),
        rows.iter()
            .filter(|r| r.claude == ClaudeState::NeedsYou)
            .count(),
    )
}

/// What a group's tabs use together: the sum of each figure, a GPU one
/// where any tab has it; `None` when no tab has a reading.
fn summed<'a>(uses: impl IntoIterator<Item = &'a Use>) -> Option<Use> {
    let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    uses.into_iter().fold(None, |sum: Option<Use>, u| {
        let Some(s) = sum else { return Some(*u) };
        Some(Use {
            cpu_pct: s.cpu_pct + u.cpu_pct,
            mem_mb: s.mem_mb + u.mem_mb,
            gpu_mb: add(s.gpu_mb, u.gpu_mb),
            gpu_pct: add(s.gpu_pct.map(u64::from), u.gpu_pct.map(u64::from))
                .map(|p| p.min(100) as u32),
        })
    })
}

/// Whether a group has a finish you have not seen: a row wearing the tick,
/// alone ([`Mark::Tick`]) or beside what runs after it ([`Mark::badged`]).
/// Asked of the rows' marks, so the header ticks exactly when a row under
/// it does — not for a tab not started this run, nor an exited shell.
fn ticks(rows: &[RowData]) -> bool {
    rows.iter().any(|r| {
        let mark = Mark::of(r);
        mark == Mark::Tick || mark.badged(r)
    })
}

/// A repository always gets the same colour, whichever order the groups come
/// out in and whichever machine it is on.
fn repo_color(chrome: &crate::chrome::Chrome, path: Option<&Path>) -> Color32 {
    let Some(path) = path else {
        return Color32::GRAY;
    };
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in path.as_os_str().as_encoded_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    chrome.category(hash as usize)
}

/// The rail's rows, grouped the way the user asked for.
fn groups(app: &App) -> Vec<GroupData> {
    match app.rail_view() {
        RailView::Categories => app
            .ws
            .categories
            .iter()
            .map(|c| {
                let color = app.chrome.category(c.color_index);
                let rows: Vec<RowData> = app
                    .ws
                    .tabs_in(c.id)
                    .map(|t| row_data(app, t, color, None))
                    .collect();
                let (count, busy, needs) = tallies(&rows);
                GroupData {
                    key: GroupKey::Category(c.id),
                    category: c.id,
                    cwd: None,
                    name: c.name.clone(),
                    color,
                    collapsed: c.collapsed,
                    count,
                    busy,
                    needs,
                    used: summed(rows.iter().filter_map(|r| r.used.as_ref())),
                    unseen: ticks(&rows),
                    rows,
                }
            })
            .collect(),
        RailView::Repos => {
            // Rail order within a repository, repositories in their own
            // order: by name, with the tabs that are in no repository last —
            // they are the leftovers, not a place.
            let mut keys: Vec<Option<PathBuf>> = Vec::new();
            for t in &app.ws.tabs {
                if !keys.contains(&t.git_repo) {
                    keys.push(t.git_repo.clone());
                }
            }
            keys.sort_by_key(|k| {
                (
                    k.is_none(),
                    k.as_deref()
                        .and_then(repo_name)
                        .unwrap_or_default()
                        .to_lowercase(),
                )
            });
            keys.into_iter()
                .map(|key| {
                    let color = repo_color(&app.chrome, key.as_deref());
                    let rows: Vec<RowData> = app
                        .ws
                        .tabs
                        .iter()
                        .filter(|t| t.git_repo == key)
                        .map(|t| row_data(app, t, color, key.as_deref()))
                        .collect();
                    let (count, busy, needs) = tallies(&rows);
                    let category = rows
                        .first()
                        .and_then(|r| app.ws.tab(r.id))
                        .map(|t| t.category)
                        .or_else(|| app.ws.categories.first().map(|c| c.id))
                        .unwrap_or(CategoryId(0));
                    GroupData {
                        name: key
                            .as_deref()
                            .and_then(repo_name)
                            .unwrap_or_else(|| "no repo".to_string()),
                        collapsed: app.repo_collapsed(key.as_deref()),
                        cwd: key.clone(),
                        key: GroupKey::Repo(key),
                        category,
                        color,
                        count,
                        busy,
                        needs,
                        used: summed(rows.iter().filter_map(|r| r.used.as_ref())),
                        unseen: ticks(&rows),
                        rows,
                    }
                })
                .collect()
        }
    }
}

/// A repository's display name: the directory it lives in.
fn repo_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
}

pub fn show(app: &mut App, ui: &mut Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    app.row_rects.clear();
    let animate = app.cfg.rail.animate;
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(ANIMATE_ID), animate));

    // Precollect display data so rendering never borrows the workspace.
    let cats = groups(app);

    let dim = app.chrome.dim;
    let fg = app.chrome.fg;

    // Bottom section first (panel-inside-panel): hooks banner + usage meters.
    let bottom = egui::Panel::bottom("rail-bottom")
        .resizable(false)
        .show_separator_line(true)
        .show(ui, |ui| {
            wslg_banner(app, ui, &mut actions);
            update_banner(app, ui, &mut actions);
            hooks_banner(app, ui, &mut actions);
            usage_panel(app, ui, dim, &mut actions)
        });
    // The line over the total row: the one the panel draws over itself,
    // with the same painter, stroke and span.
    if let Some(y) = bottom.inner {
        ui.painter().hline(
            bottom.response.rect.x_range(),
            y,
            ui.visuals().widgets.noninteractive.bg_stroke,
        );
    }

    view_switch(app, ui, &mut actions);

    // Background agents sit above the categories: they have no tab, which is
    // exactly why they are easy to forget.
    jobs_section(app, ui, dim, fg, &mut actions);

    // Drag-and-drop bookkeeping: where would a drop land right now?
    // Only in the categories view — dragging a tab into a repository would be
    // asking the rail to move a directory.
    let arrangeable = app.rail_view() == RailView::Categories;
    let pointer = ui
        .input(|i| i.pointer.interact_pos())
        .filter(|_| arrangeable);
    let mut drop_target: Option<(CategoryId, usize, f32)> = None;
    // Where a dragged *category* would land: an index in rail order, and the
    // line to draw for it.
    let mut category_drop: Option<(usize, f32)> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(6.0);
            for (position, cat) in cats.iter().enumerate() {
                let header = category_header(app, ui, cat, dim, &mut actions);
                // Above a header's middle drops before this category, below it
                // after — the same rule as tabs, one level up.
                if let Some(p) = pointer
                    && app.dragging_category.is_some()
                    && p.y >= header.min.y - 8.0
                    && p.y <= header.max.y + 8.0
                {
                    let after = p.y > header.center().y;
                    let y = if after { header.max.y } else { header.min.y };
                    category_drop = Some((position + usize::from(after), y));
                }
                // Dropping on a header (or an empty category's band) appends
                // to that category.
                if let Some(p) = pointer
                    && app.dragging.is_some()
                    && header.contains(p)
                {
                    drop_target = Some((cat.category, cat.rows.len(), header.max.y));
                }
                if !cat.collapsed {
                    for (index, row) in cat.rows.iter().enumerate() {
                        let rect = tab_row(app, ui, row, fg, dim, &mut actions);
                        // Above a row's midpoint drops before it, below after.
                        if let Some(p) = pointer
                            && app.dragging.is_some()
                            && p.x >= rect.min.x
                            && p.x <= rect.max.x
                            && p.y >= rect.min.y - 2.0
                            && p.y <= rect.max.y + 2.0
                        {
                            let after = p.y > rect.center().y;
                            let y = if after { rect.max.y } else { rect.min.y };
                            drop_target = Some((cat.category, index + usize::from(after), y));
                        }
                    }
                }
                ui.add_space(6.0);
            }
            // One more header's height, in the categories' own rhythm: the
            // next category, not yet named. A repository group is not made
            // by hand, so the repos view has none.
            if arrangeable {
                new_category_row(app, ui, &mut actions);
            }
            ui.add_space(8.0);
        });

    // A category being dragged: its own line, its own commit.
    if let Some(dragged) = app.dragging_category {
        if let Some((_, y)) = category_drop {
            let x0 = ui.min_rect().min.x + 4.0;
            let x1 = ui.min_rect().max.x - 4.0;
            ui.painter()
                .hline(x0..=x1, y, egui::Stroke::new(2.5, app.chrome.accent));
        }
        ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
        if ui.input(|i| i.pointer.any_released()) {
            if let Some((index, _)) = category_drop {
                // Taking it out first shifts everything after it up by one.
                let from = cats
                    .iter()
                    .position(|c| c.key == GroupKey::Category(dragged))
                    .unwrap_or(index);
                let index = index - usize::from(index > from);
                actions.push(Action::ReorderCategory(dragged, index));
            }
            app.dragging_category = None;
        }
    }

    // Drop indicator + commit on release.
    if let Some(dragged) = app.dragging {
        if let Some((_, _, y)) = drop_target {
            let x0 = ui.min_rect().min.x + 6.0;
            let x1 = ui.min_rect().max.x - 6.0;
            ui.painter()
                .hline(x0..=x1, y, egui::Stroke::new(2.0, app.chrome.accent));
        }
        ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
        if ui.input(|i| i.pointer.any_released()) {
            if let Some((cat, index, _)) = drop_target {
                actions.push(Action::ReorderTab(dragged, cat, index));
            }
            app.dragging = None;
        }
    }

    actions
}

/// The two ways to read the rail, at the top of it.
///
/// A segmented pair rather than a menu: which one you are in is the thing you
/// need to know at a glance, and switching is a click, not a decision. The
/// selected half is filled in its own accent; the other is quiet until the
/// pointer is on it.
fn view_switch(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    const H: f32 = 24.0;
    let c = app.chrome;
    let current = app.rail_view();

    ui.add_space(8.0);
    let width = ui.available_width() - 16.0;
    let (outer, _) = ui.allocate_exact_size(Vec2::new(width + 16.0, H), Sense::hover());
    let track = Rect::from_min_size(
        Pos2::new(outer.min.x + 8.0, outer.min.y),
        Vec2::new(width, H),
    );
    let p = ui.painter_at(outer);
    p.rect_filled(
        track,
        6.0,
        Color32::from_rgba_unmultiplied(255, 255, 255, 10),
    );

    let half = width / 2.0;
    for (index, (view, label)) in [
        (RailView::Categories, "categories"),
        (RailView::Repos, "repos"),
    ]
    .into_iter()
    .enumerate()
    {
        let seg = Rect::from_min_size(
            Pos2::new(track.min.x + half * index as f32, track.min.y),
            Vec2::new(half, H),
        );
        let resp = ui.interact(seg, ui.id().with(("rail-view", index)), Sense::click());
        let selected = current == view;
        if selected {
            p.rect_filled(seg.shrink(2.0), 5.0, c.accent.gamma_multiply(0.20));
        } else if resp.hovered() {
            p.rect_filled(
                seg.shrink(2.0),
                5.0,
                Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            );
        }
        p.text(
            seg.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(11.0),
            if selected {
                c.accent
            } else if resp.hovered() {
                c.fg
            } else {
                c.dim
            },
        );
        if resp.hovered() {
            ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
        }
        if resp.clicked() && !selected {
            actions.push(Action::SetRailView(view));
        }
    }
    ui.add_space(2.0);
}

/// Background agents, above the categories. Same grammar as tabs — spinner
/// while working, flag when it wants you — because it is the same question.
fn jobs_section(app: &App, ui: &mut Ui, dim: Color32, fg: Color32, actions: &mut Vec<Action>) {
    use giverny_claude::jobs::JobState;
    if app.claude.jobs.is_empty() {
        return;
    }
    let c = app.chrome;
    let needs: usize = app
        .claude
        .jobs
        .iter()
        .filter(|j| j.state.needs_you())
        .count();

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("BACKGROUND")
                .font(FontId::monospace(9.5))
                .color(dim),
        );
        ui.label(
            egui::RichText::new(format!("{}", app.claude.jobs.len()))
                .font(FontId::monospace(9.5))
                .color(dim),
        );
        // The section's colour, as its rows' marks are.
        if needs > 0 {
            ui.label(
                egui::RichText::new(format!("{needs}⚑"))
                    .font(FontId::monospace(9.5))
                    .color(c.accent),
            );
        }
    });

    let turns = spin_turns(ui);
    let animate = animating(ui);
    let mark = BusyMark::of(&app.cfg.rail);
    for job in &app.claude.jobs {
        let working = job.live && job.state == JobState::Working;
        let (glyph, color) = match job.state {
            // Only a *live* worker gets a spinner. A state file that still
            // says "working" after its process died would otherwise spin
            // forever, which is worse than saying nothing.
            // Off, or calm, its place stays blank: the glyph is drawn
            // see-through, so the name beside it does not move when either
            // switch flips. A calm one is drawn over that place.
            JobState::Working if job.live && (!animate || mark == BusyMark::Breath) => {
                (braille(turns), Color32::TRANSPARENT)
            }
            JobState::Working if job.live => (braille(turns), c.accent),
            JobState::Working => ("·".into(), dim),
            // The section's colour, as its working and finished marks are:
            // the tab rows' flags no longer wear amber either.
            JobState::Blocked => ("⚑".into(), c.accent),
            JobState::Done => ("✓".into(), c.accent),
            JobState::Unknown => ("·".into(), dim),
        };
        let resp = ui.horizontal(|ui| {
            ui.add_space(8.0);
            let glyph = ui.label(
                egui::RichText::new(glyph)
                    .font(FontId::monospace(11.0))
                    .color(color),
            );
            if working && mark == BusyMark::Breath {
                calm_ring(
                    ui.painter(),
                    glyph.rect.center(),
                    calm_breath(animate.then(anim_time)),
                    c.accent,
                );
            }
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(egui_line(&truncate_chars(&job.name, 22), None))
                        .font(FontId::monospace(11.5))
                        .color(if job.live { fg } else { dim }),
                );
                let mut sub = match job.state {
                    JobState::Working if job.live && job.tasks > 0 => {
                        format!(
                            "{} task{}",
                            job.tasks,
                            if job.tasks == 1 { "" } else { "s" }
                        )
                    }
                    JobState::Working if !job.live => "stale".into(),
                    JobState::Blocked => "needs you".into(),
                    JobState::Done => "done".into(),
                    _ => String::new(),
                };
                if let Some(detail) = &job.detail {
                    sub = if sub.is_empty() {
                        detail.clone()
                    } else {
                        format!("{sub} · {detail}")
                    };
                }
                if !sub.is_empty() {
                    ui.label(
                        egui::RichText::new(egui_line(&truncate_chars(&sub, 30), None))
                            .font(FontId::monospace(9.5))
                            .color(dim),
                    );
                }
            });
        });
        if working {
            match mark {
                BusyMark::Spinner => keep_spinning(ui, resp.response.rect),
                BusyMark::Breath => keep_animating(ui, resp.response.rect),
            }
        }
        let resp = resp.response.interact(Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let hover = match job.resume_target() {
            Some(_) => "open a tab attached to this agent",
            None => "no conversation recorded for this agent",
        };
        let resp = resp.on_hover_text(format!(
            "{}\n{}",
            job.cwd
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            hover
        ));
        if resp.clicked() && job.resume_target().is_some() {
            actions.push(Action::AttachJob(Box::new(job.clone())));
        }
    }
    ui.add_space(4.0);
    ui.separator();
}

fn category_header(
    app: &mut App,
    ui: &mut Ui,
    cat: &GroupData,
    dim: Color32,
    actions: &mut Vec<Action>,
) -> Rect {
    let c = app.chrome;
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::click_and_drag());
    let p = ui.painter_at(rect);

    let as_category = match &cat.key {
        GroupKey::Category(id) => Some(*id),
        GroupKey::Repo(_) => None,
    };
    // Only a repository's name fits in the rail, and two checkouts of the
    // same project have the same name.
    let resp = match &cat.key {
        GroupKey::Repo(Some(path)) => resp.on_hover_text(giverny_core::short_path(path, 60)),
        _ => resp,
    };

    // Inline rename?
    if let Some((RenameTarget::Category(id), buf)) = &mut app.rename
        && as_category == Some(*id)
    {
        let id = *id;
        // Its text (4px in from the field's edge) where the name was.
        let edit_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 16.0, rect.min.y + 2.0),
            Vec2::new(width - 24.0, HEADER_H - 4.0),
        );
        let te = ui.put(
            edit_rect,
            TextEdit::singleline(buf).font(FontId::monospace(12.0)),
        );
        if app.rename_needs_focus {
            te.request_focus();
            app.rename_needs_focus = false;
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            actions.push(Action::CommitRename(RenameTarget::Category(id), None));
        } else if enter || te.lost_focus() {
            let value = buf.clone();
            actions.push(Action::CommitRename(
                RenameTarget::Category(id),
                Some(value),
            ));
        }
        return rect;
    }

    // Right-click: what there is to manage. A repository group has nothing
    // to rename, recolour or delete — it exists because tabs are there.
    resp.context_menu(|ui| {
        if let Some(id) = as_category
            && ui.button("rename").clicked()
        {
            actions.push(Action::StartRename(RenameTarget::Category(id)));
            ui.close();
        }
        if ui.button("new tab here").clicked() {
            actions.push(Action::NewTab {
                category: cat.category,
                cwd: cat.cwd.clone(),
            });
            ui.close();
        }
        let Some(id) = as_category else { return };
        ui.menu_button("account", |ui| {
            if ui.button("(inherit)").clicked() {
                actions.push(Action::SetCategoryProfile(id, None));
                ui.close();
            }
            for p in &app.claude.profiles {
                if ui.button(format!("@{}", p.name)).clicked() {
                    actions.push(Action::SetCategoryProfile(id, Some(p.config_dir.clone())));
                    ui.close();
                }
            }
        });
        ui.menu_button("color", |ui| {
            ui.horizontal(|ui| {
                for (i, colour) in c.cats.iter().enumerate() {
                    let btn = egui::Button::new("  ").fill(*colour).corner_radius(3.0);
                    if ui.add_sized(Vec2::splat(18.0), btn).clicked() {
                        actions.push(Action::SetCategoryColor(id, i));
                        ui.close();
                    }
                }
            });
        });
        ui.separator();
        if ui.button("delete category").clicked() {
            actions.push(Action::DeleteCategory(id));
            ui.close();
        }
    });

    // The fold triangle wears the group's colour, as its name and badges
    // do: a repository group's is its own hashed colour.
    let tri = if cat.collapsed { "▸" } else { "▾" };
    p.text(
        Pos2::new(rect.min.x + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        tri,
        FontId::monospace(10.0),
        cat.color,
    );
    // A collapsed group's tabs' use, summed, at the right edge as on a tab
    // row. While the pointer is on the header the "+" takes the corner, as
    // a tab's close button does, and the sum steps left of it rather than
    // hiding. Its counts sit left of where it goes then, so nothing else
    // moves on hover.
    let hovered = ui.rect_contains_pointer(rect);
    let used = cat.used.filter(|_| cat.collapsed);
    let badge_x = match &used {
        Some(u) => {
            let y = rect.center().y;
            let right = if hovered {
                rect.max.x - 26.0
            } else {
                rect.max.x - 8.0
            };
            readout(&p, right, [y - 6.0, y + 6.0], u, c);
            rect.max.x - 26.0 - readout_width(u) - 4.0
        }
        None => rect.max.x - 26.0,
    };
    // The working dot breathes, so the header asks for frames while it shows
    // (a collapsed group draws no tab rows that would).
    if cat.busy > 0 && cat.needs == 0 {
        keep_animating(ui, rect);
    }
    let breath = calm_breath(animating(ui).then(anim_time));
    let name_end = header_badges(&p, rect.center().y, badge_x, cat, dim, breath);
    // The name, cut to what fits before them (the rail is monospace), right
    // after the fold triangle: no dot between them, the name's own colour is
    // the category's.
    let name_font = FontId::monospace(11.5);
    let char_w = ui
        .ctx()
        .fonts_mut(|f| f.glyph_width(&name_font, 'M'))
        .max(1.0);
    let name_x = rect.min.x + 20.0;
    let char_budget = ((name_end - 6.0 - name_x) / char_w).max(4.0) as usize;
    p.text(
        Pos2::new(name_x, rect.center().y),
        Align2::LEFT_CENTER,
        truncate_chars(&cat.name.to_uppercase(), char_budget),
        name_font,
        cat.color,
    );

    // "+" new-tab zone at the right edge.
    let plus_rect = Rect::from_min_size(
        Pos2::new(rect.max.x - 22.0, rect.min.y + 3.0),
        Vec2::splat(20.0),
    );
    let plus = ui.interact(
        plus_rect,
        ui.id().with(("cat-plus", &cat.name, cat.category.0)),
        Sense::click(),
    );
    if used.is_none() || hovered {
        p.text(
            plus_rect.center(),
            Align2::CENTER_CENTER,
            "+",
            FontId::monospace(13.0),
            if plus.hovered() { cat.color } else { dim },
        );
    }
    // Dragging a header reorders categories; a repository group is where its
    // tabs are and cannot be moved by hand.
    if let Some(id) = as_category {
        if resp.drag_started() {
            app.dragging_category = Some(id);
            app.dragging = None;
        }
        if app.dragging_category == Some(id) {
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 4.0, cat.color.gamma_multiply(0.12));
        }
    }
    if plus.clicked() {
        actions.push(Action::NewTab {
            category: cat.category,
            cwd: cat.cwd.clone(),
        });
    } else if resp.clicked() {
        actions.push(match &cat.key {
            GroupKey::Category(id) => Action::ToggleCollapse(*id),
            // The group for tabs in no repository is keyed by the empty path,
            // which is not a path anything else can be.
            GroupKey::Repo(repo) => Action::ToggleRepoCollapse(repo.clone().unwrap_or_default()),
        });
    }
    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
    rect
}

/// A group header's right-aligned summary, right to left from `badge_x`:
/// its tab count, then **one** mark: the flag and how many
/// tabs want you if any do, else the dot and how many work if any do,
/// else nothing. Wanting you outranks working, and two marks side by side
/// were more than a header line says at a glance. Returns where the name
/// has to end, left of the leftmost badge, so it takes the room a missing
/// mark leaves.
///
/// A finish you have not seen in the group ([`GroupData::unseen`]) adds
/// the tick a tab row wears as a badge: left of the dot and its count, so
/// the dot keeps its place, or alone in the dot's place when nothing
/// works. Not beside the flag, which wins as it does on a row: what you
/// owe matters more than what you missed. The tick has no count; the rows
/// say which tabs it is.
///
/// Every mark is the group's colour: the flag
/// and its count were amber, which matched nothing else in the header. The
/// working mark is a dot that goes from empty to full and back on the calm
/// breath (`breath`, [`calm_breath`]): a fixed outline of [`HEADER_DOT_R`],
/// always drawn, whose centre fades in from nothing at 0
/// to the outline's colour at 1, where it holds with `rail.animate` off.
/// It never changes size. Its caller keeps the frames coming.
fn header_badges(
    p: &egui::Painter,
    y: f32,
    mut badge_x: f32,
    cat: &GroupData,
    dim: Color32,
    breath: f32,
) -> f32 {
    let digits = |n: usize| n.to_string().len() as f32;
    p.text(
        Pos2::new(badge_x, y),
        Align2::RIGHT_CENTER,
        format!("{}", cat.count),
        FontId::monospace(10.0),
        dim,
    );
    let name_end = badge_x - 6.0 * digits(cat.count);
    badge_x -= 20.0;
    if cat.needs > 0 {
        flag(p, Pos2::new(badge_x - 3.0, y), cat.color);
        p.text(
            Pos2::new(badge_x - 9.0, y),
            Align2::RIGHT_CENTER,
            format!("{}", cat.needs),
            FontId::monospace(10.0),
            cat.color,
        );
        return badge_x - 9.0 - 6.0 * digits(cat.needs);
    }
    // Where the tick would sit, and what is left of it before then.
    let (tick_x, end) = if cat.busy > 0 {
        // One circle: its outline always there, its centre the colour
        // faded in by the breath, the stroke drawn over the fill's edge.
        p.circle(
            Pos2::new(badge_x, y),
            HEADER_DOT_R - RING_STROKE / 2.0,
            cat.color.gamma_multiply(breath.clamp(0.0, 1.0)),
            Stroke::new(RING_STROKE, cat.color),
        );
        p.text(
            Pos2::new(badge_x - 8.0, y),
            Align2::RIGHT_CENTER,
            format!("{}", cat.busy),
            FontId::monospace(10.0),
            cat.color,
        );
        let end = badge_x - 8.0 - 6.0 * digits(cat.busy);
        (end - HEADER_TICK_GAP, end)
    } else {
        (badge_x, name_end)
    };
    if !cat.unseen {
        return end;
    }
    badge_tick(p, Pos2::new(tick_x, y), cat.color);
    tick_x - HEADER_TICK_GAP
}

/// From a header tick's centre to the count beside it, and to where the
/// name may end: half the badge tick's width and enough air that the tick
/// does not read as part of the dot's count.
const HEADER_TICK_GAP: f32 = 8.0;

/// The header's working dot, its outer edge: a touch inside the calm
/// ring's [`RING_R`] so the two read as one size beside each other.
const HEADER_DOT_R: f32 = 3.5;

/// "+ add category": the row under the last category that adds one, drawn
/// as a ghost of [`category_header`] rather than a framed button that looked
/// dropped in. The same height, font and places: a `+` where
/// the fold triangle sits, its words where a name starts. Dim and frameless
/// at rest; hovered, it takes a tab row's faint highlight and the colour the
/// new category will wear (the palette's next, as `add_category` picks it),
/// so the row previews what a click makes. Lower case, unlike the names
/// above it: it is an offer, not a category.
fn new_category_row(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let c = app.chrome;
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::click());
    let p = ui.painter_at(rect);
    // `hovered` and not the pointer's position: a tab or category being
    // dragged over it is not aimed at it.
    let hovered = resp.hovered();
    if hovered {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 8),
        );
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
    let color = if hovered {
        c.category(app.ws.categories.len())
    } else {
        c.dim
    };
    // Centred on the header's fold triangle, which starts 8 in.
    let tri_font = FontId::monospace(10.0);
    let tri_w = ui.ctx().fonts_mut(|f| f.glyph_width(&tri_font, '▾'));
    p.text(
        Pos2::new(rect.min.x + 8.0 + tri_w / 2.0, rect.center().y),
        Align2::CENTER_CENTER,
        "+",
        FontId::monospace(11.5),
        color,
    );
    p.text(
        Pos2::new(rect.min.x + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        "add category",
        FontId::monospace(11.5),
        color,
    );
    if resp.clicked() {
        actions.push(Action::NewCategory);
    }
}

/// A ring with a gap, turned `turns` of the way round (see [`spin`]). What
/// the braille spinner was for, minus the assumption that the font has
/// braille in it.
fn spinner(p: &egui::Painter, at: Pos2, turns: f64, color: Color32) {
    const R: f32 = 5.0;
    let head = (turns.fract() * std::f64::consts::TAU) as f32;
    let mut points = Vec::with_capacity(14);
    for step in 0..14 {
        let angle = head + step as f32 * 0.36;
        points.push(at + Vec2::new(angle.cos() * R, angle.sin() * R));
    }
    // Fades towards the tail, so which way it turns is legible at 5px.
    for (i, pair) in points.windows(2).enumerate() {
        let strength = 0.25 + 0.75 * (i as f32 / 13.0);
        p.line_segment(
            [pair[0], pair[1]],
            Stroke::new(1.6, color.gamma_multiply(strength)),
        );
    }
}

/// A pennant on a staff: this tab wants you.
fn flag(p: &egui::Painter, at: Pos2, color: Color32) {
    let staff = at + Vec2::new(-3.0, -6.0);
    p.line_segment(
        [staff, staff + Vec2::new(0.0, 12.0)],
        Stroke::new(1.4, color),
    );
    p.add(egui::Shape::convex_polygon(
        vec![
            staff + Vec2::new(0.5, 0.0),
            staff + Vec2::new(7.0, 3.0),
            staff + Vec2::new(0.5, 6.0),
        ],
        color,
        Stroke::NONE,
    ));
}

/// A tick: finished while you were elsewhere.
fn tick(p: &egui::Painter, at: Pos2, color: Color32) {
    tick_scaled(p, at, 1.0, 1.8, color);
}

/// How far left of a tab's mark the badge tick sits: clear of the widest
/// mark beside it (the spinner, the three dots), inside the gutter before
/// the active-tab bar.
const BADGE_OFFSET: f32 = 13.0;

/// The tick as a badge beside a mark that is still running:
/// three quarters the size and a touch thinner, so it reads as a note on
/// the mark and not a second mark.
fn badge_tick(p: &egui::Painter, at: Pos2, color: Color32) {
    tick_scaled(p, at, 0.75, 1.5, color);
}

/// A tick at `at`, `scale` times the full one, stroked `width` wide.
fn tick_scaled(p: &egui::Painter, at: Pos2, scale: f32, width: f32, color: Color32) {
    let stroke = Stroke::new(width, color);
    let at_ = |x: f32, y: f32| at + Vec2::new(x, y) * scale;
    p.line_segment([at_(-4.5, 0.0), at_(-1.5, 3.5)], stroke);
    p.line_segment([at_(-1.5, 3.5), at_(4.5, -4.0)], stroke);
}

/// A plain shell's mark at `at`: a square `side` points
/// across, filled or hollow. Every Claude mark is round or drawn — spinner,
/// calm ring, flag, tick, idle dot, three dots — so a square says "just a
/// shell" by shape alone, now that colour says the category for both.
/// Snapped to whole device pixels, edges and all: a square straddling a
/// pixel boundary smears into a blurred blob at this size, where a circle
/// only softens.
fn square(p: &egui::Painter, at: Pos2, side: f32, filled: bool, color: Color32) {
    let hole = if filled {
        0.0
    } else {
        side - 2.0 * square_stroke(p.pixels_per_point())
    };
    square_band(p, at, side, hole, color);
}

/// A hollow square's edge, in points: a whole number of pixels, at least
/// one, at `ppp` pixels per point.
fn square_stroke(ppp: f32) -> f32 {
    (1.2 * ppp).round().max(1.0) / ppp
}

/// A square `outer` points across with a square hole `hole` across in its
/// middle, snapped to whole device pixels: the band between is drawn as a
/// stroke inside the edge, so as the hole closes the stroke widens until it
/// fills the square. A hole under a pixel is drawn filled, which it
/// already looks.
fn square_band(p: &egui::Painter, at: Pos2, outer: f32, hole: f32, color: Color32) {
    let ppp = p.pixels_per_point();
    let px = (outer * ppp).round().max(1.0);
    let min = Pos2::new(
        (at.x * ppp - px / 2.0).round() / ppp,
        (at.y * ppp - px / 2.0).round() / ppp,
    );
    let rect = Rect::from_min_size(min, Vec2::splat(px / ppp));
    // Each side of the hole loses the same whole number of pixels.
    let width = ((px - (hole * ppp).max(0.0)) / 2.0).round().max(1.0);
    if px - 2.0 * width < 1.0 {
        p.rect_filled(rect, 0.0, color);
    } else {
        p.rect_stroke(
            rect,
            0.0,
            Stroke::new(width / ppp, color),
            egui::StrokeKind::Inside,
        );
    }
}

/// A tab's status mark: its shape says what the tab is
/// doing, its colour which category it is in. Claude's marks are round or
/// drawn; a plain shell's are square, so the two no longer pass for each
/// other (an exited shell's ring was an idle Claude's, a live shell's dot
/// the calm busy one's).
///
/// The tick says "finished while you were elsewhere", for both. Alone it is
/// the whole mark: nothing runs after the finish you have not seen. When
/// something does — the agent working again, its subagents or background
/// shell humming on, a shell's next command — that is the mark, and the
/// tick rides beside it as a badge ([`Mark::badged`]) until you look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Claude working: the spinner, or with `rail.calm_busy` the idle dot
    /// breathing out to an open ring and back.
    Busy,
    /// Claude wants you: the pulsing flag. It wins over everything, the
    /// badge too: what you owe it matters more than what you missed.
    Flag,
    /// Something finished while you were elsewhere, and nothing runs after
    /// it: the tick. A Claude turn that ended with its subagents or a
    /// background shell still running is not done yet, seen or unseen, so
    /// it wears [`Mark::Wave`] until they end, with the tick beside it
    /// while unseen. A shell whose command ended
    /// on another tab ticks the same.
    Tick,
    /// Claude idle at its prompt, seen: a small filled dot in the
    /// category's colour, the size of a not-started Claude tab's gray one.
    Idle,
    /// Claude at its prompt, free to talk, while something it started
    /// hums on — a background shell or its subagents: the three-dot wave,
    /// whether or not you have seen the turn that left them running. The
    /// agent itself working, which it cannot talk through, is
    /// [`Mark::Busy`], and wins; it asking for you is [`Mark::Flag`], and
    /// wins too.
    Wave,
    /// A plain shell running a command: [`Mark::Prompt`]'s small filled
    /// square breathing out to the hollow square and back.
    Running,
    /// A plain shell at its prompt, its last command seen: a small filled
    /// square, as Claude at its prompt is a small dot.
    Prompt,
    /// A plain shell that has exited: a small hollow square in gray, the
    /// size of a shell at its prompt, not the running breath's top; hollow,
    /// so it differs from the filled gray square of a tab not started —
    /// nothing runs there.
    Exited,
    /// A tab restored from a previous run whose process has not spawned:
    /// tiny and gray, a circle when it will come back as Claude, a square
    /// when it will come back as a shell.
    NotStarted { claude: bool },
}

impl Mark {
    /// A still shell mark's square, (side, filled): at its prompt and not
    /// started, the small filled one; exited, the small hollow one.
    fn still_square(self) -> Option<(f32, bool)> {
        match self {
            Mark::Prompt | Mark::NotStarted { claude: false } => Some((SMALL_SQUARE, true)),
            Mark::Exited => Some((SMALL_SQUARE, false)),
            _ => None,
        }
    }

    /// The mark `row` wears. A tab that has not started has nothing
    /// running, so whatever Claude state lingers for it is not shown: the
    /// gray mark is the whole truth until it is focused.
    fn of(row: &RowData) -> Mark {
        if !row.started && !row.exited {
            return Mark::NotStarted {
                claude: row.resumes_claude,
            };
        }
        let mark = match row.claude {
            ClaudeState::Busy => Mark::Busy,
            ClaudeState::NeedsYou => Mark::Flag,
            // Unseen or not, a turn whose work still runs is not done: the
            // wave says so, the state staying `DoneUnseen` until you look (a
            // manager's tab ticked while its worker built), and the tick
            // waits beside it ([`Mark::badged`]).
            ClaudeState::DoneUnseen | ClaudeState::Idle if row.background => Mark::Wave,
            ClaudeState::DoneUnseen => Mark::Tick,
            ClaudeState::Idle => Mark::Idle,
            ClaudeState::None if row.exited => Mark::Exited,
            ClaudeState::None if row.running => Mark::Running,
            ClaudeState::None => Mark::Prompt,
        };
        // Back at rest with a finish still unseen — a shell's command, or a
        // turn the agent has since followed with another — the tick alone,
        // as for a turn that ended once.
        match mark {
            Mark::Idle | Mark::Prompt if row.unseen => Mark::Tick,
            mark => mark,
        }
    }

    /// Whether the tick rides beside this mark: something
    /// finished unseen, and something runs on after it — the agent working
    /// again, what it left humming, a shell's next command. Not beside the
    /// flag, which wins, nor an exited shell, which runs nothing.
    fn badged(self, row: &RowData) -> bool {
        row.unseen && matches!(self, Mark::Busy | Mark::Wave | Mark::Running)
    }

    /// What it is drawn in: the tab's category `color`, Claude or shell,
    /// except a tab that has not started or a shell that has exited, which
    /// is `dim` — gray says "nothing runs here" in any category.
    fn color(self, color: Color32, dim: Color32) -> Color32 {
        match self {
            Mark::NotStarted { .. } | Mark::Exited => dim,
            _ => color,
        }
    }
}

fn tab_row(
    app: &mut App,
    ui: &mut Ui,
    row: &RowData,
    fg: Color32,
    dim: Color32,
    actions: &mut Vec<Action>,
) -> Rect {
    let c = app.chrome;
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    // Geometry-based hover: `resp.hovered()` flickers when the close button
    // overlays the row in the hit-test stack.
    let hovered = ui.rect_contains_pointer(rect);
    // Remembered for file drags, which are tracked outside egui and so have
    // to hit-test themselves.
    app.row_rects.push((rect, row.id));

    // A file drag aimed at this row: say so before the drop, since the drop
    // types into whichever tab is under the pointer.
    if app.drag_hover.is_some_and(|p| rect.contains(p)) {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            c.accent.gamma_multiply(0.22),
        );
        p.rect_stroke(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Stroke::new(1.0, c.accent),
            egui::StrokeKind::Inside,
        );
    }

    if row.active {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            row.color.gamma_multiply(0.16),
        );
        p.rect_filled(
            Rect::from_min_size(rect.min + Vec2::new(4.0, 1.0), Vec2::new(3.0, ROW_H - 2.0)),
            2.0,
            row.color,
        );
    } else if hovered {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 8),
        );
    }

    // Status mark (see [`Mark`]): Claude state wins over the plain shell square.
    //
    // Drawn, not typed. These were characters — a braille spinner, a braille
    // ⠿ for a background shell — and a font without the braille block turns
    // them into empty boxes, which is what every Windows machine did. A shape
    // is a shape everywhere.
    // Indented past the header above it: a category's name starts at 20, so
    // its tabs start after that. Outdented children read as a list that lost
    // its heading.
    let dot = Pos2::new(rect.min.x + 32.0, rect.min.y + 13.0);
    let time = anim_time();
    let mark = Mark::of(row);
    let color = mark.color(row.color, dim);
    match mark {
        // Off, a working tab's spinner is not drawn, the only state left
        // blank (a calm ring holds still, full size). The title stays
        // where it is.
        Mark::Busy => busy(ui, &p, rect, dot, color, BusyMark::of(&app.cfg.rail)),
        Mark::Flag => {
            // One breath every two seconds, sampled at the animation step.
            // Off, no pulse: the flag held at full colour.
            let pulse = if animating(ui) {
                keep_animating(ui, rect);
                breath(time)
            } else {
                1.0
            };
            flag(&p, dot, color.gamma_multiply(pulse));
        }
        // Still: finished, nothing owed. The flag above is the one that
        // wants something; the two differ in shape and movement.
        Mark::Tick => tick(&p, dot, color),
        // A background shell or its subagents keep running while Claude
        // waits at its prompt. Worth showing — it is why a directory is
        // still churning — but it is not the agent working: it can take a
        // message. So no spinner and no flag: a slow colour drifting
        // through three small dots, the hum of something left running.
        // Off, they hold still and faded.
        Mark::Wave => shell_dots(ui, &p, rect, dot, color),
        Mark::Idle => {
            p.circle_filled(dot, DOT_R, color);
        }
        // A plain shell, which most tabs are most of the time: a small
        // filled square at its prompt, breathing out to the hollow square
        // and back while a command runs, and the small square hollow and in
        // gray once the shell has exited. Squares read larger than circles
        // of the same width, so each is a touch narrower than its round twin.
        Mark::Running => breathing_square(ui, &p, rect, dot, color),
        Mark::Prompt | Mark::Exited | Mark::NotStarted { claude: false } => {
            if let Some((side, filled)) = mark.still_square() {
                square(&p, dot, side, filled, color);
            }
        }
        // A restored tab whose process has not started yet: small and
        // gray, since any colour would claim something running.
        Mark::NotStarted { claude: true } => {
            p.circle_filled(dot, DOT_R, color);
        }
    }
    // The unseen finish beside what runs after it: the tick,
    // smaller, in the gutter left of the mark, so the mark keeps its place
    // and the title never moves as the badge comes and goes.
    if mark.badged(row) {
        badge_tick(&p, dot - Vec2::new(BADGE_OFFSET, 0.0), color);
    }

    // Inline rename?
    if let Some((RenameTarget::Tab(id), buf)) = &mut app.rename
        && *id == row.id
    {
        let edit_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 44.0, rect.min.y + 3.0),
            Vec2::new(width - 56.0, 20.0),
        );
        let te = ui.put(
            edit_rect,
            TextEdit::singleline(buf).font(FontId::monospace(12.0)),
        );
        if app.rename_needs_focus {
            te.request_focus();
            app.rename_needs_focus = false;
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            actions.push(Action::CommitRename(RenameTarget::Tab(row.id), None));
        } else if enter || te.lost_focus() {
            let value = buf.clone();
            actions.push(Action::CommitRename(RenameTarget::Tab(row.id), Some(value)));
        }
        return rect;
    }

    // Its shell's CPU over its memory at the right edge, as the account
    // bars' numbers are drawn, each coloured by its share of the whole
    // machine. While the pointer is on the row the close button takes the
    // corner and the figures fold onto the second line, CPU left of memory:
    // they stay in sight, and the sub line gives way.
    let sub_y = rect.min.y + 29.0;
    let mut sub_end = rect.max.x - 8.0;
    if let Some(u) = &row.used {
        if hovered {
            sub_end = readout_inline(&p, rect.max.x - 8.0, sub_y, u, c) - 6.0;
        } else {
            readout(&p, rect.max.x - 8.0, [rect.min.y + 13.0, sub_y], u, c);
        }
    }

    // Title (char-budget truncation; the rail is monospace).
    let reserved = row.used.as_ref().map_or(0.0, readout_width);
    let char_budget = ((width - 68.0 - reserved) / 7.2).max(4.0) as usize;
    p.text(
        Pos2::new(rect.min.x + 44.0, rect.min.y + 13.0),
        Align2::LEFT_CENTER,
        egui_line(&truncate_chars(&row.title, char_budget), None),
        FontId::monospace(12.5),
        if row.active {
            fg
        } else {
            fg.gamma_multiply(0.8)
        },
    );
    if !row.sub.is_empty() {
        let sub_font = FontId::monospace(10.0);
        let sub_budget = if hovered && row.used.is_some() {
            let char_w = ui
                .ctx()
                .fonts_mut(|f| f.glyph_width(&sub_font, 'M'))
                .max(1.0);
            ((sub_end - (rect.min.x + 44.0)) / char_w).max(1.0) as usize
        } else {
            char_budget + 2
        };
        p.text(
            Pos2::new(rect.min.x + 44.0, sub_y),
            Align2::LEFT_CENTER,
            truncate_chars(&row.sub, sub_budget),
            sub_font,
            dim,
        );
    }

    // Close button on hover.
    if hovered {
        let close_rect = Rect::from_min_size(
            Pos2::new(rect.max.x - 24.0, rect.min.y + 4.0),
            Vec2::splat(18.0),
        );
        let close = ui.interact(
            close_rect,
            ui.id().with(("tab-close", row.id.0)),
            Sense::click(),
        );
        p.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::monospace(13.0),
            if close.hovered() {
                Color32::from_rgb(0xd9, 0x7f, 0x70)
            } else {
                dim
            },
        );
        if close.clicked() {
            actions.push(Action::CloseTab(row.id));
            return rect;
        }
    }

    // Right-click: tab management.
    resp.context_menu(|ui| {
        if ui.button("rename").clicked() {
            actions.push(Action::StartRename(RenameTarget::Tab(row.id)));
            ui.close();
        }
        if ui.button("sessions…").clicked() {
            actions.push(Action::OpenSessions(row.id));
            ui.close();
        }
        ui.menu_button("move to", |ui| {
            for (id, name) in &app
                .ws
                .categories
                .iter()
                .map(|c| (c.id, c.name.clone()))
                .collect::<Vec<_>>()
            {
                if ui.button(name).clicked() {
                    actions.push(Action::MoveTab(row.id, *id));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("close").clicked() {
            actions.push(Action::CloseTab(row.id));
            ui.close();
        }
    });

    if resp.drag_started() {
        app.dragging = Some(row.id);
    }
    if resp.double_clicked() {
        actions.push(Action::StartRename(RenameTarget::Tab(row.id)));
    } else if resp.clicked() {
        actions.push(Action::Select(row.id));
    } else if resp.middle_clicked() {
        actions.push(Action::CloseTab(row.id));
    }
    if hovered {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
    // The dragged row dims in place; the drop indicator shows where it lands.
    if app.dragging == Some(row.id) {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Color32::from_rgba_unmultiplied(0, 0, 0, 90),
        );
    }
    rect
}

/// WSLg lost its shared memory with Windows: say why the window lags and
/// hand over the commands that fix it, the one that needs no administrator
/// first. Red, since it is a failure the user has to act on, and there until
/// dismissed; nothing when healthy.
fn wslg_banner(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let Some([fix, fix_admin]) = app.wslg_slow else {
        return;
    };
    let c = app.chrome;
    ui.add_space(4.0);
    egui::Frame::NONE
        .stroke(Stroke::new(1.0, c.poppy))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::same(6))
        .outer_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(
                egui::RichText::new(
                    "⚠ WSLg lost its shared memory with Windows, so every frame \
                     is sent the slow way and typing will lag.",
                )
                .font(FontId::monospace(10.0))
                .color(c.poppy),
            );
            let step = |ui: &mut Ui, text: &str| {
                ui.label(
                    egui::RichText::new(text)
                        .font(FontId::monospace(10.0))
                        .color(c.dim),
                );
            };
            step(
                ui,
                "1. In any PowerShell or cmd, no administrator needed. It ends \
                 all of WSL, this window and its Claude sessions included, so \
                 reopen Giverny after:",
            );
            wslg_fix_command(ui, &c, fix, "wslg-fix");
            step(ui, "2. Still slow? In PowerShell as administrator:");
            wslg_fix_command(ui, &c, fix_admin, "wslg-fix-admin");
            if ui
                .small_button("dismiss")
                .on_hover_text("WSLg stays slow until the fix is run")
                .clicked()
            {
                actions.push(Action::DismissWslgSlow);
            }
        });
}

/// One of [`wslg_banner`]'s commands, selectable and wrapping, with a copy
/// button of its own that says when it has copied.
fn wslg_fix_command(ui: &mut Ui, c: &crate::chrome::Chrome, command: &str, id: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let copied_id = ui.id().with(id).with("copied");
        let copied = ui.data(|d| d.get_temp::<bool>(copied_id).unwrap_or(false));
        if ui
            .small_button(if copied { "copied" } else { "copy" })
            .on_hover_text("copy the command")
            .clicked()
        {
            ui.ctx().copy_text(command.to_string());
            ui.data_mut(|d| d.insert_temp(copied_id, true));
        }
        egui::Frame::NONE
            .fill(c.fg.gamma_multiply(0.08))
            .corner_radius(3.0)
            .inner_margin(egui::Margin::symmetric(4, 2))
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(command)
                            .font(FontId::monospace(10.5))
                            .color(c.fg),
                    )
                    .wrap()
                    .selectable(true),
                );
            });
    });
}

/// Offer the new release the hourly check found, follow the install it
/// starts, and then offer the restart that finishes it.
///
/// It was a line of 10px text with a small button beside it, in a rail full of
/// small text, and it went unseen for versions at a time. A release nobody
/// installs is a release nobody has.
fn update_banner(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let c = app.chrome;
    let version = app.update.as_ref().map(|u| u.version.as_str());
    // The installer has already replaced the binary: the only thing left is
    // handing over to it, and that offer outlives any dismissal. It breathes
    // toward the foreground, opaque throughout, so its ink stays readable.
    if app.update_installed {
        let fill = c
            .amber
            .lerp_to_gamma(c.fg, (1.0 - breath(anim_time())) * 0.35);
        let rect = banner_button(
            ui,
            &match version {
                Some(v) => format!("⟳ restart to finish v{v}"),
                None => "⟳ restart to finish the update".into(),
            },
            fill,
            c.panel,
            "the new version is on disk.\nthis closes every tab and opens them again,\nresuming the claude sessions in them",
            actions,
            Action::RestartNow,
        );
        keep_animating(ui, rect);
        return;
    }
    // Between the click and the binary landing: the install is in a tab, and
    // this row says so and leads there. The spinner is the tab rows' drawn
    // one, not a braille character (see `spinner`).
    if let Some(job) = &app.update_job {
        let rect = banner_button(
            ui,
            &match version {
                Some(v) => format!("installing v{v}…"),
                None => "installing the update…".into(),
            },
            c.accent.lerp_to_gamma(c.panel, 0.6),
            c.fg,
            "installing in its tab, click to watch",
            actions,
            Action::Select(job.tab),
        );
        let at = Pos2::new(rect.min.x + 13.0, rect.center().y);
        if let Some(turns) = spin(ui, rect) {
            spinner(ui.painter(), at, turns, c.fg);
        }
        return;
    }
    let Some(available) = &app.update else { return };
    if app.update_dismissed {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let width = (ui.available_width() - 28.0).max(60.0);
        let button = egui::Button::new(
            egui::RichText::new(format!("▲ update to v{}", available.version))
                .font(FontId::monospace(11.5))
                .color(c.panel),
        )
        .fill(c.accent)
        .corner_radius(4.0);
        let what =
            "opens a tab and runs the official install command,\nso you see exactly what runs";
        if ui
            .add_sized(Vec2::new(width, 22.0), button)
            .on_hover_text(if app.update_stalled {
                format!(
                    "{}\n\nthe last attempt ended without installing it.\n{what}",
                    available.url
                )
            } else {
                format!("{}\n\n{what}", available.url)
            })
            .clicked()
        {
            actions.push(Action::RunUpdate);
        }
        if ui.small_button("×").on_hover_text("not now").clicked() {
            actions.push(Action::DismissUpdate);
        }
    });
}

/// One button across the width of the rail. Returns where it was drawn, for
/// whoever animates it.
fn banner_button(
    ui: &mut Ui,
    text: &str,
    fill: Color32,
    ink: Color32,
    hover: &str,
    actions: &mut Vec<Action>,
    action: Action,
) -> Rect {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let width = (ui.available_width() - 6.0).max(60.0);
        let button = egui::Button::new(
            egui::RichText::new(text)
                .font(FontId::monospace(11.5))
                .color(ink),
        )
        .fill(fill)
        .corner_radius(4.0);
        let resp = ui.add_sized(Vec2::new(width, 22.0), button);
        let rect = resp.rect;
        if resp.on_hover_text(hover).clicked() {
            actions.push(action);
        }
        rect
    })
    .inner
}

fn hooks_banner(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    if app.claude.profiles.is_empty() {
        return;
    }
    let c = app.chrome;
    if app.claude.hooks_installed && !app.claude.relay_listening() {
        // Another live Giverny is the usual reason, and the one worth naming:
        // every hook goes to its window, and this one shows nothing.
        let (text, hover) = match app.claude.relay_holder() {
            Some(holder) => (
                match holder.pid {
                    Some(pid) => format!("⚠ relay held by Giverny pid {pid}"),
                    None => "⚠ relay held by another Giverny".to_string(),
                },
                "This window opened while another Giverny was running, and every \
                 Claude session's hooks go to that one: no live states here. Close \
                 this window and use that one; a deliberate second window wants its \
                 own XDG_RUNTIME_DIR.",
            ),
            None => (
                "⚠ hook relay socket failed — states degraded".to_string(),
                "The hook relay socket could not be bound; `giverny doctor` says more.",
            ),
        };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(text)
                    .font(FontId::monospace(10.0))
                    .color(c.poppy),
            )
            .on_hover_text(hover);
        });
    }
    // Hooks were just installed for the first time, and Claude Code reads
    // them when a session starts: the sessions already running report
    // nothing. Only those; the hint goes when the last of them ends.
    let without = app.claude.sessions_without_hooks();
    if without > 0 {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("⟳ restart claude for live states")
                    .font(FontId::monospace(10.0))
                    .color(c.amber),
            )
            .on_hover_text(format!(
                "{without} claude session(s) started before the hooks were installed\n\
                 and report no states until restarted; new sessions have them."
            ));
        });
    }
    if app.claude.hooks_installed || app.hooks_banner_dismissed {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("⚑ live Claude states need hooks")
                .font(FontId::monospace(10.0))
                .color(c.amber),
        );
        if ui.small_button("install").on_hover_text(format!(
            "adds `giverny relay` to {} event(s) plus a compact statusline for live usage,\nin each profile's settings.json (existing hooks and any statusline of your own\nare preserved; a .giverny-bak backup is written)",
            giverny_claude::hooks::RELAY_EVENTS.len()
        )).clicked() {
            actions.push(Action::InstallHooks);
        }
        if ui.small_button("×").clicked() {
            actions.push(Action::DismissHooksBanner);
        }
    });
}

/// Codex uses the same account row and meter geometry as Claude.
fn codex_accounts(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    if !app.cfg.codex.enabled {
        return;
    }
    let c = app.chrome;
    let now = jiff::Timestamp::now();
    for account in &app.codex.accounts {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let name = account.name.as_deref().unwrap_or("default");
            ui.label(
                egui::RichText::new(format!("@{name}"))
                    .font(FontId::monospace(10.5))
                    .color(c.fg),
            )
            .on_hover_text(match &account.target.wsl_distro {
                Some(d) => format!("{} · {d}", account.target.home.display()),
                None => account.target.home.display().to_string(),
            });
            if account.refreshing {
                let response = ui.label(
                    egui::RichText::new(braille(spin_turns(ui)))
                        .font(FontId::monospace(9.0))
                        .color(c.accent),
                );
                keep_spinning(ui, response.rect);
            } else if let Some(updated) = account.updated {
                let minutes = updated.elapsed().as_secs() / 60;
                let age = if minutes < 1 {
                    "· just now".into()
                } else if minutes < 120 {
                    format!("· {minutes}m old")
                } else if minutes < 1440 {
                    format!("· {}h old", minutes / 60)
                } else {
                    format!("· {}d old", minutes / 1440)
                };
                ui.label(egui::RichText::new(age).font(FontId::monospace(9.0)).color(
                    if account.error.is_some() || minutes > 30 {
                        c.amber
                    } else {
                        c.dim
                    },
                ))
                .on_hover_text("last successful reading from Codex");
            }
            if let Some(error) = &account.error {
                ui.label(
                    egui::RichText::new("⚠")
                        .font(FontId::monospace(9.0))
                        .color(c.amber),
                )
                .on_hover_text(error);
            }
        });
        for window in &account.windows {
            let expired = window.resets_at.is_some_and(|at| at <= now.as_second());
            let read = crate::claude_watch::Reading {
                percent: window.used_percent,
                live: false,
                critical: window.used_percent >= 100.0,
                resets: window
                    .resets_at
                    .and_then(|s| jiff::Timestamp::from_second(s).ok()),
            };
            let label = window.label.strip_prefix("codex ").unwrap_or(&window.label);
            ui.scope(|ui| usage_bar(ui, label, !expired, read, now, c.dim, c.fg, c))
                .response
                .on_hover_text(format!(
                    "{}{}",
                    window.label,
                    if expired || account.error.is_some() {
                        " · last reading is stale"
                    } else {
                        ""
                    }
                ));
        }
        if account.windows.is_empty() && !account.refreshing {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(if account.error.is_some() {
                        "usage unavailable"
                    } else {
                        "no usage data yet"
                    })
                    .font(FontId::monospace(9.5))
                    .color(c.dim),
                )
                .on_hover_text(
                    account
                        .error
                        .as_deref()
                        .unwrap_or("Refresh usage to request a reading from Codex."),
                );
            });
        }
    }
    for ready in app.codex.pending_restore.values() {
        let title = app.ws.tab(ready.id).map(|t| t.title()).unwrap_or("tab");
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if ui
                .small_button(
                    egui::RichText::new(format!("resume · {}", truncate_chars(title, 20)))
                        .font(FontId::monospace(9.5))
                        .color(c.dim),
                )
                .on_hover_text("Resume this saved Codex conversation.")
                .clicked()
            {
                actions.push(Action::ConfirmCodexResume(ready.id));
            }
        });
    }
}

/// Returns where the line over the total row goes, when there is one.
fn usage_panel(app: &App, ui: &mut Ui, dim: Color32, actions: &mut Vec<Action>) -> Option<f32> {
    let c = app.chrome;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("ACCOUNTS")
                .font(FontId::monospace(9.5))
                .color(dim),
        );
        let spinning = app.claude.refresh_in_flight()
            || (app.cfg.codex.enabled && app.codex.accounts.iter().any(|a| a.refreshing));
        let label = if spinning && animating(ui) {
            braille(spin_turns(ui))
        } else {
            "⟳".to_string()
        };
        let refresh = ui.add(egui::Button::new(
            egui::RichText::new(label)
                .font(FontId::monospace(10.0))
                .color(if spinning { c.accent } else { dim }),
        ));
        if spinning {
            keep_spinning(ui, refresh.rect);
        }
        if refresh
            .on_hover_text(
                "refresh account usage\n(Claude Code updates its cache; Codex fetches its allowance windows)",
            )
            .clicked()
        {
            actions.push(Action::RefreshUsage);
            if app.cfg.codex.enabled { actions.push(Action::RefreshCodexUsage); }
        }
        // The way in that does not require knowing a chord.
        if ui
            .add(egui::Button::new(
                egui::RichText::new("⚙")
                    .font(FontId::monospace(11.0))
                    .color(dim),
            ))
            .on_hover_text("settings  (Ctrl+,)")
            .clicked()
        {
            actions.push(Action::ToggleSettings);
        }
    });
    if app.cfg.codex.enabled && !app.codex.accounts.is_empty() {
        provider_accounts(ui, "CODEX", c, |ui| codex_accounts(app, ui, actions));
    }
    if !app.claude.accounts.is_empty() {
        provider_accounts(ui, "CLAUDE", c, |ui| claude_accounts(app, ui, actions));
    } else if !app.cfg.codex.enabled {
        ui.label(
            egui::RichText::new("no accounts found")
                .font(FontId::monospace(10.0))
                .color(dim),
        );
        ui.add_space(6.0);
        return None;
    }
    ui.add_space(6.0);
    // Off (`rail.total_line`), no line and no rule over it.
    if !app.cfg.rail.total_line {
        return None;
    }
    let r = app.use_now.clone()?;
    Some(sessions_load_row(ui, &r, app.chrome))
}

/// A rotated label interrupts the left category line; its height comes from
/// the accounts, so provider headings add no rows to the rail.
fn provider_accounts(
    ui: &mut Ui,
    label: &str,
    c: crate::chrome::Chrome,
    draw: impl FnOnce(&mut Ui),
) {
    ui.horizontal_top(|ui| {
        let (gutter, _) = ui.allocate_exact_size(Vec2::new(12.0, 0.0), Sense::hover());
        let content = ui
            .vertical(|ui| {
                ui.set_width(ui.available_width());
                draw(ui);
            })
            .response
            .rect;
        let x = gutter.center().x;
        let center = content.center().y;
        let font_size = (content.height() / label.len() as f32 / 0.65).clamp(6.5, 9.0);
        let galley = ui
            .painter()
            .layout_no_wrap(label.into(), FontId::monospace(font_size), c.dim);
        let half = galley.size().x / 2.0;
        let stroke = Stroke::new(1.0, c.dim.gamma_multiply(0.35));
        if center - half - 2.0 > content.top() + 1.0 {
            ui.painter().line_segment(
                [
                    Pos2::new(x, content.top() + 1.0),
                    Pos2::new(x, center - half - 2.0),
                ],
                stroke,
            );
        }
        if center + half + 2.0 < content.bottom() - 1.0 {
            ui.painter().line_segment(
                [
                    Pos2::new(x, center + half + 2.0),
                    Pos2::new(x, content.bottom() - 1.0),
                ],
                stroke,
            );
        }
        let position = Pos2::new(x - galley.size().y / 2.0, center + half);
        ui.painter().add(
            egui::epaint::TextShape::new(position, galley, c.dim)
                .with_angle(-std::f32::consts::FRAC_PI_2),
        );
    });
}

fn claude_accounts(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let c = app.chrome;
    let dim = c.dim;
    let fg = c.fg;
    if !app.claude.accounts.is_empty() && !app.claude.statusline_on() {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if ui
                .small_button("enable live usage")
                .on_hover_text(
                    "adds a compact statusline to claude that pushes usage to Giverny\n\
                     (official rate_limits field — no API calls)",
                )
                .clicked()
            {
                actions.push(Action::ToggleStatusline(true));
            }
        });
    }
    if app.claude.accounts.is_empty() && !app.cfg.codex.enabled {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("no claude profiles found")
                    .font(FontId::monospace(10.0))
                    .color(dim),
            );
        });
        ui.add_space(6.0);
        return;
    }
    let now = jiff::Timestamp::now();
    for acc in &app.claude.accounts {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("@{}", acc.profile.name))
                    .font(FontId::monospace(10.5))
                    .color(fg),
            );
            // Say where the numbers came from and how old they are — a
            // cache age shown next to live-pushed bars reads as stale.
            let human = |m: i64| {
                if m >= 1440 {
                    format!("{}d", m / 1440)
                } else if m >= 120 {
                    format!("{}h", m / 60)
                } else {
                    format!("{m}m")
                }
            };
            match ClaudeWatch::freshness(acc, now) {
                Freshness::Live(m) => {
                    ui.label(
                        egui::RichText::new(if m < 2 {
                            "· live".to_string()
                        } else {
                            format!("· live {}", human(m))
                        })
                        .font(FontId::monospace(9.0))
                        .color(c.accent),
                    )
                    .on_hover_text("pushed by claude's statusline");
                }
                Freshness::Cache(m) => {
                    // Always, not only once it is stale: "how old is this"
                    // was unanswerable for the first half hour, which is
                    // exactly when the number is being watched.
                    let resp = ui.label(
                        egui::RichText::new(if m < 1 {
                            "· just now".to_string()
                        } else {
                            format!("· {} old", human(m))
                        })
                        .font(FontId::monospace(9.0))
                        .color(if m > 30 { c.amber } else { dim }),
                    );
                    resp.on_hover_text("from claude's on-disk cache");
                }
                _ => {}
            }
        });
        match &acc.usage {
            Some(u) if !u.limits.is_empty() => {
                for limit in &u.limits {
                    let read = ClaudeWatch::reading(acc, limit, now);
                    usage_bar(ui, &limit.label(), limit.is_active, read, now, dim, fg, c);
                }
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("no usage data yet")
                            .font(FontId::monospace(9.5))
                            .color(dim),
                    );
                });
            }
        }
    }
    ui.add_space(6.0);
}

#[allow(clippy::too_many_arguments)]
fn usage_bar(
    ui: &mut Ui,
    label: &str,
    active: bool,
    read: crate::claude_watch::Reading,
    now: jiff::Timestamp,
    dim: Color32,
    fg: Color32,
    c: crate::chrome::Chrome,
) {
    let pct = read.percent;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 15.0), Sense::hover());
    let p = ui.painter_at(rect);
    let color = if read.critical {
        c.poppy
    } else if pct >= 80.0 {
        c.amber
    } else {
        c.dim
    };

    // Label.
    p.text(
        Pos2::new(rect.min.x + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        truncate_chars(label, 6),
        FontId::monospace(9.5),
        if active { fg } else { dim },
    );
    // Track + fill.
    let track = Rect::from_min_max(
        Pos2::new(rect.min.x + 58.0, rect.center().y - 3.0),
        Pos2::new(rect.max.x - 74.0, rect.center().y + 3.0),
    );
    if track.width() > 10.0 {
        p.rect_filled(
            track,
            3.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 14),
        );
        let mut fill = track;
        fill.set_right(track.min.x + track.width() * (pct as f32 / 100.0));
        if fill.width() > 0.5 {
            p.rect_filled(fill, 3.0, color);
        }
    }
    // Numbers. A leading dot marks a value pushed live by the statusline.
    let mut right = format!("{}{:>3.0}%", if read.live { "·" } else { " " }, pct);
    if let Some(at) = read.resets {
        let cd = giverny_claude::usage::countdown_to(at, now);
        right = format!("{right} {cd:>6}");
    }
    p.text(
        Pos2::new(rect.max.x - 6.0, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::monospace(9.0),
        if read.critical { c.poppy } else { dim },
    );
}

/// Under the account bars: what Giverny runs now, summed —
/// `23% CPU  2.0G RAM  40% GPU`, centred, in the bars' lettering. Hovering
/// says what is counted. Returns where the line above it goes.
fn sessions_load_row(
    ui: &mut Ui,
    r: &giverny_claude::use_reading::Reading,
    c: crate::chrome::Chrome,
) -> f32 {
    let dim = c.dim;
    let y = ui.cursor().min.y + 0.5;
    ui.add_space(5.0);
    let width = ui.available_width();
    let (mut rect, resp) = ui.allocate_exact_size(Vec2::new(width, 15.0), Sense::hover());
    // A wide row (a banner, a button) can stretch the panel's layout past
    // what is shown: the figures end at the visible edge.
    rect.max.x = rect.max.x.min(ui.clip_rect().max.x);
    // Each figure coloured by its share of the whole machine, as a tab's
    // are.
    let mut job = egui::text::LayoutJob::default();
    let ram = crate::sessions_load::machine_ram_mb();
    for (text, share) in crate::sessions_load::figure_parts(&r.total, ram) {
        job.append(
            &text,
            0.0,
            egui::TextFormat::simple(FontId::monospace(9.5), machine_color(share, c)),
        );
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let at = rect.center() - galley.size() / 2.0;
    ui.painter_at(rect).galley(at, galley, dim);
    ui.add_space(5.0);
    let gpu = match (r.total.gpu_pct, r.total.gpu_mb) {
        (Some(_), _) => ", the GPUs' compute it uses",
        (None, Some(_)) => ", GPU memory (this GPU reports no per-process utilisation)",
        (None, None) => "",
    };
    resp.on_hover_text(format!(
        "Everything Giverny runs now: the app and every tab, Claude or not —\n\
         {} Claude Code session{} with all they started (commands, builds).\n\
         CPU as a share of the whole machine, the memory it really uses\n\
         (shared pages split){gpu}. Each tab's figures are parts of this\n\
         same reading.",
        r.claudes,
        if r.claudes == 1 { "" } else { "s" },
    ));
    y
}

/// A readout's column: four or five 9px monospace figures and some air.
const READOUT_COL: f32 = 34.0;

/// How wide [`readout`] draws `u`: a column, two with a GPU.
fn readout_width(u: &Use) -> f32 {
    if u.gpu_pct.is_some() || u.gpu_mb.is_some() {
        2.0 * READOUT_COL
    } else {
        READOUT_COL
    }
}

/// A tab's (or a collapsed group's) use, right-aligned to `right`: CPU at
/// `ys[0]` over memory at `ys[1]` and, with a GPU, its compute over its
/// memory in a column to their left, each figure coloured by its share of
/// the whole machine ([`machine_color`]; GPU memory, whose total is not
/// known here, stays dim).
fn readout(p: &egui::Painter, right: f32, ys: [f32; 2], u: &Use, c: crate::chrome::Chrome) {
    use giverny_claude::session_use::gb;
    let (cpu, mem) =
        crate::sessions_load::machine_shares(u, crate::sessions_load::machine_ram_mb());
    let gpu_x = right - READOUT_COL;
    let figures = [
        Some((right, ys[0], format!("{}%", u.cpu_pct.min(100)), cpu)),
        Some((right, ys[1], gb(u.mem_mb), mem)),
        u.gpu_pct
            .map(|g| (gpu_x, ys[0], format!("g{}%", g.min(100)), f64::from(g))),
        u.gpu_mb.map(|m| (gpu_x, ys[1], format!("g{}", gb(m)), 0.0)),
    ];
    for (x, y, text, share) in figures.into_iter().flatten() {
        p.text(
            Pos2::new(x, y),
            Align2::RIGHT_CENTER,
            text,
            FontId::monospace(9.0),
            machine_color(share, c),
        );
    }
}

/// [`readout`] folded onto one line for a hovered tab row:
/// `g40% g1.2G  23%  1.2G`, right-aligned to `right` at `y`, coloured as
/// there. Returns where it starts.
fn readout_inline(p: &egui::Painter, right: f32, y: f32, u: &Use, c: crate::chrome::Chrome) -> f32 {
    use giverny_claude::session_use::gb;
    let (cpu, mem) =
        crate::sessions_load::machine_shares(u, crate::sessions_load::machine_ram_mb());
    // Right to left, each with the gap to draw before the next one.
    let figures = [
        Some((gb(u.mem_mb), mem, 10.0)),
        Some((format!("{}%", u.cpu_pct.min(100)), cpu, 10.0)),
        u.gpu_mb.map(|m| (format!("g{}", gb(m)), 0.0, 4.0)),
        u.gpu_pct
            .map(|g| (format!("g{}%", g.min(100)), f64::from(g), 4.0)),
    ];
    let mut x = right;
    let mut gap = 0.0;
    for (text, share, after) in figures.into_iter().flatten() {
        let at = p.text(
            Pos2::new(x - gap, y),
            Align2::RIGHT_CENTER,
            text,
            FontId::monospace(9.0),
            machine_color(share, c),
        );
        x = at.min.x;
        gap = after;
    }
    x
}

/// How much of the whole machine a figure is, as the rail colours it: dim
/// while [`Load::Light`](crate::sessions_load::Load), amber or poppy above.
fn machine_color(share: f64, c: crate::chrome::Chrome) -> Color32 {
    crate::sessions_load::Load::of(share).color(c.dim, c)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delay the rail asks for in one frame that draws a spinner and a
    /// pulse on screen, with `rail.animate` at `animate`, and the spinner's
    /// angle (`None`: no spinner drawn).
    fn rail_wake(ctx: &egui::Context, animate: bool) -> (std::time::Duration, Option<f64>) {
        let mut turns = None;
        let out = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new(ANIMATE_ID), animate));
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(10.0));
            turns = spin(ui, rect);
            keep_spinning(ui, rect);
            keep_animating(ui, rect);
        });
        let delay = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        (delay, turns)
    }

    /// Off, there is no spinner and the rail wakes nothing; on, it spins
    /// and asks for the clock's next tick (#43).
    #[test]
    fn a_rail_without_animation_draws_no_spinner_and_schedules_no_frames() {
        let ctx = egui::Context::default();
        for _ in 0..3 {
            rail_wake(&ctx, false);
        }
        let (delay, turns) = rail_wake(&ctx, false);
        assert_eq!(
            delay,
            std::time::Duration::MAX,
            "a still rail woke the window"
        );
        assert_eq!(turns, None, "a spinner drawn with the switch off");
        let (delay, turns) = rail_wake(&ctx, true);
        assert!(delay < std::time::Duration::from_secs(1), "{delay:?}");
        assert!(turns.is_some(), "no spinner with the switch on");
    }

    /// A figure is coloured by its share of the whole machine: dim under
    /// 20 %, amber from 20 % to 50 %, poppy over 50 %. Memory is taken of
    /// all the RAM: 51 % of it is poppy, as 51 % CPU is.
    #[test]
    fn a_figure_is_coloured_by_its_share_of_the_machine() {
        let c =
            crate::chrome::Chrome::from_theme(&giverny_term::render::theme::Theme::monet_dark());
        assert!(c.dim != c.amber && c.amber != c.poppy && c.dim != c.poppy);
        for (share, want) in [
            (0.0, c.dim),
            (19.0, c.dim),
            (19.9, c.dim),
            (20.0, c.amber),
            (29.0, c.amber),
            (50.0, c.amber),
            (50.5, c.poppy),
            (51.0, c.poppy),
            (100.0, c.poppy),
        ] {
            assert_eq!(machine_color(share, c), want, "{share}%");
        }
        let ram = 24_000;
        let at = |cpu_pct, mem_mb| {
            let u = Use {
                cpu_pct,
                mem_mb,
                gpu_mb: None,
                gpu_pct: None,
            };
            let (cpu, mem) = crate::sessions_load::machine_shares(&u, ram);
            (machine_color(cpu, c), machine_color(mem, c))
        };
        assert_eq!(at(19, 4_560), (c.dim, c.dim), "19 %");
        assert_eq!(at(20, 4_800), (c.amber, c.amber), "20 %");
        assert_eq!(at(25, 6_000), (c.amber, c.amber), "25 %");
        assert_eq!(at(50, 12_000), (c.amber, c.amber), "50 %");
        assert_eq!(at(51, 12_240), (c.poppy, c.poppy), "51 %");
        // Each by its own share.
        assert_eq!(at(51, 100), (c.poppy, c.dim));
    }

    /// A collapsed group's figure is its tabs' summed; a GPU figure where
    /// any tab has one; nothing where no tab reads.
    #[test]
    fn a_group_sums_its_tabs() {
        let u = |cpu_pct, mem_mb, gpu_mb| Use {
            cpu_pct,
            mem_mb,
            gpu_mb,
            gpu_pct: None,
        };
        assert_eq!(summed(&[]), None);
        assert_eq!(summed(&[u(3, 100, None)]), Some(u(3, 100, None)));
        assert_eq!(
            summed(&[u(3, 100, None), u(40, 2048, Some(512)), u(0, 5, None)]),
            Some(u(43, 2153, Some(512)))
        );
        assert_eq!(
            summed(&[u(1, 1, None), u(1, 1, None)]).unwrap().gpu_mb,
            None
        );
    }

    #[test]
    fn a_repository_is_named_by_its_directory() {
        assert_eq!(
            repo_name(Path::new("/home/ita/work/orbital-api")).as_deref(),
            Some("orbital-api")
        );
        assert_eq!(repo_name(Path::new("/")), None);
    }

    /// The colour is the group's identity in the rail; it has to be the same
    /// one every launch, and two repositories should rarely share it.
    #[test]
    fn a_repository_keeps_its_colour() {
        let a = Path::new("/home/ita/work/orbital-api");
        let b = Path::new("/home/ita/work/atlas-web");
        let chrome =
            crate::chrome::Chrome::from_theme(&giverny_term::render::theme::Theme::monet_dark());
        assert_eq!(repo_color(&chrome, Some(a)), repo_color(&chrome, Some(a)));
        assert_ne!(repo_color(&chrome, Some(a)), repo_color(&chrome, Some(b)));
        // Tabs in no repository are grouped, not coloured.
        assert_eq!(repo_color(&chrome, None), Color32::GRAY);
    }

    #[test]
    fn the_busy_mark_breathes_unless_asked_to_spin() {
        let mut cfg = giverny_core::config::RailConfig::default();
        assert_eq!(
            BusyMark::of(&cfg),
            BusyMark::Breath,
            "the breath by default"
        );
        cfg.calm_busy = false;
        assert_eq!(BusyMark::of(&cfg), BusyMark::Spinner);
    }

    /// One breath every three seconds, out of step with the amber flag's two,
    /// and still when the rail does not move.
    #[test]
    fn the_calm_mark_breathes_slower_than_the_flag() {
        let at = |t: f64| calm_breath(Some(t));
        assert!(at(0.0) < 0.01, "starts breathed out");
        assert!((at(1.5) - 1.0).abs() < 0.01, "in at half a breath");
        for t in [0.25, 0.7, 1.1, 2.4] {
            assert!((at(t) - at(t + 3.0)).abs() < 1e-5, "a breath is 3s");
            assert!((0.0..=1.0).contains(&at(t)));
        }
        // The flag's pulse is back where it was after 2s; the calm mark is not.
        assert!((breath(0.25) - breath(2.25)).abs() < 1e-5);
        assert!((at(0.25) - at(2.25)).abs() > 0.2);
        assert_eq!(calm_breath(None), 1.0, "a still rail holds the dot full");
    }

    /// The calm mark wakes the window on the shared animation clock, as the
    /// pulse does, and draws in a frame without a spinner (#43).
    #[test]
    fn the_calm_mark_keeps_to_the_animation_clock() {
        let ctx = egui::Context::default();
        let mut out = None;
        for _ in 0..2 {
            out = Some(ctx.run_ui(egui::RawInput::default(), |ui| {
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(10.0));
                busy(
                    ui,
                    ui.painter(),
                    rect,
                    rect.center(),
                    Color32::WHITE,
                    BusyMark::Breath,
                );
            }));
        }
        let delay = out.unwrap().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(delay <= giverny_term::pace::STEP_BACKGROUND, "{delay:?}");
    }

    /// With `rail.animate` off a calm rail still marks a working tab: the
    /// ring, full size and still, and no frame asked for. The spinner it
    /// replaces draws nothing at all.
    #[test]
    fn a_still_rail_holds_the_calm_ring_full() {
        let ctx = egui::Context::default();
        let frame = |mark: BusyMark| {
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.ctx()
                    .data_mut(|d| d.insert_temp(egui::Id::new(ANIMATE_ID), false));
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(10.0));
                busy(ui, ui.painter(), rect, rect.center(), Color32::WHITE, mark);
            });
            let delay = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            let shapes: Vec<egui::Shape> = out.shapes.into_iter().map(|c| c.shape).collect();
            (delay, shapes)
        };
        for _ in 0..3 {
            frame(BusyMark::Breath);
        }
        let (delay, shapes) = frame(BusyMark::Breath);
        assert_eq!(
            delay,
            std::time::Duration::MAX,
            "a still ring woke the window"
        );
        // Breathed out to the open ring, all the way, and hollow.
        let (outer, inner) = band(&shapes);
        assert!((outer - (RING_R + RING_STROKE)).abs() < 0.05, "{outer}");
        assert!((inner - RING_R).abs() < 0.05, "{inner}");
        let mesh = the_mesh(&shapes);
        assert_eq!(
            ink_at(mesh, Pos2::new(5.0, 5.0), Color32::WHITE),
            0.0,
            "hollow"
        );
        assert!(mesh.vertices.iter().any(|v| v.color == Color32::WHITE));
        let (_, shapes) = frame(BusyMark::Spinner);
        assert!(shapes.is_empty(), "a spinner drawn with the switch off");
    }

    /// A colour travels through the shell's dots: each in [0,1], the peak
    /// moving 0 → 1 → 2 and back to 0 after a lap, and all dim and still
    /// when the rail does not move.
    #[test]
    fn the_shell_dots_carry_a_colour_left_to_right() {
        const LAP: f64 = 3.75;
        let at = |t: f64| shell_wave(Some(t));
        let peak = |w: [f32; 3]| (0..3).max_by(|&a, &b| w[a].total_cmp(&w[b])).unwrap();
        for k in 0..60 {
            let t = k as f64 * 0.25;
            let w = at(t);
            assert!(w.iter().all(|v| (0.0..=1.0).contains(v)), "{t}: {w:?}");
            assert!(w.iter().filter(|&&v| v > 1e-4).count() <= 2, "{t}: {w:?}");
            let later = at(t + LAP);
            for i in 0..3 {
                assert!((w[i] - later[i]).abs() < 1e-5, "a lap is {LAP}s at {t}");
            }
        }
        for (i, t) in [0.0, LAP / 3.0, 2.0 * LAP / 3.0, LAP]
            .into_iter()
            .enumerate()
        {
            let w = at(t);
            assert_eq!(peak(w), i % 3, "{t}: {w:?}");
            assert!((w[i % 3] - 1.0).abs() < 1e-5, "full at its peak: {w:?}");
        }
        // Halfway from one dot to the next, the two share the light.
        let w = at(LAP / 6.0);
        assert!(
            (w[0] - w[1]).abs() < 1e-5 && w[0] > 0.3 && w[2] == 0.0,
            "{w:?}"
        );
        // Not in step with the calm mark (3s) or the amber flag (2s).
        assert!((at(0.25)[0] - at(3.25)[0]).abs() > 0.2);
        assert!((at(0.25)[0] - at(2.25)[0]).abs() > 0.2);
        assert_eq!(shell_wave(None), [0.0; 3], "a still rail holds them dim");
    }

    /// The shell's dots: moving, they wake the window on the shared clock;
    /// with `rail.animate` off they are drawn still and faded, in the tab's
    /// colour rather than gray, and ask for no frame.
    #[test]
    fn the_shell_dots_hold_still_and_faded_when_the_rail_does_not_move() {
        let ctx = egui::Context::default();
        let faded = Color32::RED.gamma_multiply(WAVE_REST);
        let frame = |animate: bool| {
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.ctx()
                    .data_mut(|d| d.insert_temp(egui::Id::new(ANIMATE_ID), animate));
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(10.0));
                shell_dots(ui, ui.painter(), rect, rect.center(), Color32::RED);
            });
            let delay = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            let dots: Vec<egui::epaint::CircleShape> = out
                .shapes
                .into_iter()
                .filter_map(|c| match c.shape {
                    egui::Shape::Circle(c) => Some(c),
                    _ => None,
                })
                .collect();
            (delay, dots)
        };
        for _ in 0..3 {
            frame(true);
        }
        let (delay, dots) = frame(true);
        assert_eq!(dots.len(), 3);
        assert!(delay <= giverny_term::pace::STEP_BACKGROUND, "{delay:?}");
        for _ in 0..3 {
            frame(false);
        }
        let (delay, dots) = frame(false);
        assert_eq!(
            delay,
            std::time::Duration::MAX,
            "still dots woke the window"
        );
        assert_eq!(dots.len(), 3);
        for (i, d) in dots.iter().enumerate() {
            assert_eq!(d.fill, faded, "dot {i}");
            assert_eq!(d.radius, 1.4);
            assert_eq!(d.center.x, 5.0 + (i as f32 - 1.0) * 4.0);
        }
    }

    /// A group header's badges are all its colour, it shows one mark at
    /// most, the flag outranking the dot, and its working mark at full
    /// breath is a filled dot; the badges ask for no frame themselves
    /// (#43). A finish unseen in it adds the badge tick, left of the dot or
    /// alone in its place, never beside the flag. The name ends left of
    /// whatever is there.
    #[test]
    fn a_header_marks_in_its_colour_and_holds_still() {
        let group = |busy, needs, unseen| GroupData {
            key: GroupKey::Repo(None),
            category: CategoryId(0),
            cwd: None,
            name: "work".into(),
            color: Color32::RED,
            collapsed: true,
            count: 3,
            busy,
            needs,
            used: None,
            unseen,
            rows: Vec::new(),
        };
        // (busy, needs, unseen) -> (dots, flag, tick, where the name ends):
        // the count's one digit ends 6 left of 200, a mark's slot starts 20
        // left of it, and a tick takes 8 either side of its centre.
        let cases = [
            ((2, 1, false), (0, true, false, 200.0 - 20.0 - 9.0 - 6.0)),
            ((0, 1, false), (0, true, false, 200.0 - 20.0 - 9.0 - 6.0)),
            ((2, 1, true), (0, true, false, 200.0 - 20.0 - 9.0 - 6.0)),
            ((2, 0, false), (1, false, false, 200.0 - 20.0 - 8.0 - 6.0)),
            (
                (2, 0, true),
                (1, false, true, 200.0 - 20.0 - 8.0 - 6.0 - 8.0 - 8.0),
            ),
            ((0, 0, false), (0, false, false, 200.0 - 6.0)),
            ((0, 0, true), (0, false, true, 200.0 - 20.0 - 8.0)),
        ];
        let ctx = egui::Context::default();
        for ((busy, needs, unseen), (want_dots, want_flag, want_tick, want_end)) in cases {
            let cat = group(busy, needs, unseen);
            for animate in [true, false] {
                let mut out = None;
                let mut name_end = 0.0;
                for _ in 0..3 {
                    out = Some(ctx.run_ui(egui::RawInput::default(), |ui| {
                        ui.ctx()
                            .data_mut(|d| d.insert_temp(egui::Id::new(ANIMATE_ID), animate));
                        name_end =
                            header_badges(ui.painter(), 10.0, 200.0, &cat, Color32::GRAY, 1.0);
                    }));
                }
                let out = out.unwrap();
                let case =
                    format!("busy {busy}, needs {needs}, unseen {unseen}, animate {animate}");
                assert_eq!(
                    out.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
                    std::time::Duration::MAX,
                    "a header woke the window ({case})"
                );
                assert_eq!(name_end, want_end, "{case}");
                let shapes: Vec<egui::Shape> = out.shapes.into_iter().map(|c| c.shape).collect();
                let dots: Vec<_> = shapes
                    .iter()
                    .filter_map(|s| match s {
                        egui::Shape::Circle(c) => Some(c),
                        _ => None,
                    })
                    .collect();
                assert_eq!(dots.len(), want_dots, "{case}: {shapes:?}");
                for d in dots {
                    assert!((d.radius + d.stroke.width / 2.0 - HEADER_DOT_R).abs() < 1e-4);
                    assert_eq!(d.fill, Color32::RED, "full, in the group's colour");
                }
                // The flag: its staff and its pennant, neither amber.
                for s in &shapes {
                    match s {
                        egui::Shape::LineSegment { stroke, .. } => {
                            assert_eq!(stroke.color, Color32::RED, "flag staff")
                        }
                        egui::Shape::Path(path) => {
                            assert_eq!(path.fill, Color32::RED, "pennant")
                        }
                        _ => {}
                    }
                }
                assert_eq!(
                    shapes.iter().any(|s| matches!(s, egui::Shape::Path(_))),
                    want_flag,
                    "{case}: {shapes:?}"
                );
                // The badge tick: two strokes as thin as a row's badge.
                let tick_strokes = shapes
                    .iter()
                    .filter(|s| {
                        matches!(s, egui::Shape::LineSegment { stroke, .. } if stroke.width == 1.5)
                    })
                    .count();
                assert_eq!(tick_strokes, if want_tick { 2 } else { 0 }, "{case}");
            }
        }
    }

    /// The header's working dot goes from empty to full: one circle whose
    /// outline holds its size and colour while only its centre fades in,
    /// nothing at breath 0 and the outline's colour at 1, never dropping as
    /// the breath climbs.
    #[test]
    fn the_header_dot_fades_its_centre_from_empty_to_full() {
        let cat = GroupData {
            key: GroupKey::Repo(None),
            category: CategoryId(0),
            cwd: None,
            name: "x".into(),
            color: Color32::RED,
            collapsed: false,
            count: 1,
            busy: 1,
            needs: 0,
            used: None,
            unseen: false,
            rows: Vec::new(),
        };
        let at = |breath: f32| {
            let shapes = painted(|p| {
                header_badges(p, 10.0, 200.0, &cat, Color32::GRAY, breath);
            });
            let circles: Vec<egui::epaint::CircleShape> = shapes
                .into_iter()
                .filter_map(|s| match s {
                    egui::Shape::Circle(c) => Some(c),
                    _ => None,
                })
                .collect();
            assert_eq!(circles.len(), 1, "one dot, at {breath}");
            circles[0]
        };
        let mut last = 0;
        for k in 0..=100 {
            let d = at(k as f32 / 100.0);
            assert!(
                (d.radius + d.stroke.width / 2.0 - HEADER_DOT_R).abs() < 1e-4,
                "the size holds"
            );
            assert_eq!(
                d.stroke,
                Stroke::new(RING_STROKE, Color32::RED),
                "the outline holds"
            );
            assert!(d.fill.a() >= last, "the centre only fills in");
            last = d.fill.a();
        }
        assert_eq!(at(0.0).fill.a(), 0, "empty");
        assert_eq!(at(1.0).fill, Color32::RED, "full");
    }

    /// A header ticks exactly when a row under it does: a finish unseen
    /// alone or beside what runs after it, but not on a tab not started
    /// this run, nor on an exited shell, nor for what you have seen.
    #[test]
    fn a_header_ticks_when_a_row_does() {
        let unseen = |mut r: RowData| {
            r.unseen = true;
            r
        };
        assert!(ticks(&[row(ClaudeState::DoneUnseen)]));
        assert!(ticks(&[
            row(ClaudeState::Idle),
            unseen(row(ClaudeState::Busy))
        ]));
        assert!(ticks(&[unseen(row(ClaudeState::None))]));
        assert!(!ticks(&[row(ClaudeState::Idle), row(ClaudeState::Busy)]));
        let pending = RowData {
            started: false,
            ..row(ClaudeState::DoneUnseen)
        };
        assert!(!ticks(&[pending]));
        let exited = RowData {
            exited: true,
            ..unseen(row(ClaudeState::None))
        };
        assert!(!ticks(&[exited]));
    }

    /// A row in `claude` state, started, live, in no background shell.
    fn row(claude: ClaudeState) -> RowData {
        RowData {
            id: TabId(1),
            title: String::new(),
            sub: String::new(),
            active: false,
            exited: false,
            started: true,
            resumes_claude: false,
            color: Color32::RED,
            claude,
            background: false,
            running: false,
            unseen: claude == ClaudeState::DoneUnseen,
            used: None,
        }
    }

    /// Every state gets its own mark, and a plain shell's are squares no
    /// Claude state shares.
    #[test]
    fn each_state_has_its_mark_and_shells_are_square() {
        assert_eq!(Mark::of(&row(ClaudeState::Busy)), Mark::Busy);
        assert_eq!(Mark::of(&row(ClaudeState::NeedsYou)), Mark::Flag);
        assert_eq!(Mark::of(&row(ClaudeState::DoneUnseen)), Mark::Tick);
        assert_eq!(Mark::of(&row(ClaudeState::Idle)), Mark::Idle);
        let humming = RowData {
            background: true,
            ..row(ClaudeState::Idle)
        };
        assert_eq!(Mark::of(&humming), Mark::Wave);
        assert_eq!(Mark::of(&row(ClaudeState::None)), Mark::Prompt);
        let exited = RowData {
            exited: true,
            ..row(ClaudeState::None)
        };
        assert_eq!(Mark::of(&exited), Mark::Exited);
        // An exited tab never spawned this run is still exited, not pending.
        let exited_unstarted = RowData {
            started: false,
            ..exited
        };
        assert_eq!(Mark::of(&exited_unstarted), Mark::Exited);
    }

    /// The mark says whether the agent can take a message: busy, it
    /// cannot; at its prompt with subagents still working, the wave; with
    /// nothing left running, the idle dot. A turn you have not seen waves
    /// too while its work runs, and ticks only once it is all done.
    #[test]
    fn subagents_running_after_the_turn_wave_and_a_working_agent_spins() {
        use crate::claude_watch::ClaudeTab;
        let mark = |state, agents| {
            let mut tab = ClaudeTab::default();
            tab.state = state;
            tab.agents = agents;
            Mark::of(&RowData {
                background: tab.working_in_background(),
                ..row(state)
            })
        };
        assert_eq!(mark(ClaudeState::Idle, true), Mark::Wave);
        assert_eq!(mark(ClaudeState::Busy, true), Mark::Busy, "busy wins");
        assert_eq!(mark(ClaudeState::Idle, false), Mark::Idle, "agents gone");
        // The flag is unchanged by what runs behind it.
        assert_eq!(mark(ClaudeState::NeedsYou, true), Mark::Flag);
        // Unseen, the turn's workers still running: not done, the wave;
        // they end, the tick.
        assert_eq!(mark(ClaudeState::DoneUnseen, true), Mark::Wave);
        assert_eq!(mark(ClaudeState::DoneUnseen, false), Mark::Tick);
        // A background shell holds the tick back the same.
        let shell = |state| {
            Mark::of(&RowData {
                background: true,
                ..row(state)
            })
        };
        assert_eq!(shell(ClaudeState::DoneUnseen), Mark::Wave);
        assert_eq!(shell(ClaudeState::Idle), Mark::Wave);
    }

    /// A turn that ended unseen keeps its tick as a badge beside what runs
    /// after it — the wave of its workers, the agent working again — until
    /// you look; alone, with nothing running, it is the tick itself. The
    /// flag wins over all of it.
    #[test]
    fn an_unseen_turn_ticks_beside_what_runs_after_it() {
        let marked = |row: RowData| {
            let mark = Mark::of(&row);
            (mark, mark.badged(&row))
        };
        // Its workers still running: the wave, the tick beside it.
        let humming = RowData {
            background: true,
            ..row(ClaudeState::DoneUnseen)
        };
        assert_eq!(marked(humming), (Mark::Wave, true));
        // Seen, the wave alone.
        let seen = RowData {
            background: true,
            ..row(ClaudeState::Idle)
        };
        assert_eq!(marked(seen), (Mark::Wave, false));
        // Unseen, then the agent works again: the spinner (or the calm
        // ring), the tick beside it; that turn ends too, the tick alone.
        let again = RowData {
            unseen: true,
            ..row(ClaudeState::Busy)
        };
        assert_eq!(marked(again), (Mark::Busy, true));
        assert_eq!(marked(row(ClaudeState::DoneUnseen)), (Mark::Tick, false));
        // Back at its prompt without a `Stop` while still unseen: the tick.
        let idle = RowData {
            unseen: true,
            ..row(ClaudeState::Idle)
        };
        assert_eq!(marked(idle), (Mark::Tick, false));
        // Nothing unseen: no badge on the spinner.
        assert_eq!(marked(row(ClaudeState::Busy)), (Mark::Busy, false));
        // The flag wins, badge and all.
        let asks = RowData {
            unseen: true,
            ..row(ClaudeState::NeedsYou)
        };
        assert_eq!(marked(asks), (Mark::Flag, false));
    }

    /// A plain shell: a command running breathes its square;
    /// one that finished on another tab ticks until you look; at its prompt,
    /// seen, the small filled square; exited, the small hollow square in
    /// gray. Its next command, the first still unseen, breathes with the tick beside it.
    #[test]
    fn a_shell_breathes_ticks_then_rests_hollow() {
        let shell = |running, unseen, exited| {
            let row = RowData {
                running,
                unseen,
                exited,
                ..row(ClaudeState::None)
            };
            let mark = Mark::of(&row);
            (mark, mark.badged(&row))
        };
        assert_eq!(shell(true, false, false), (Mark::Running, false));
        assert_eq!(shell(false, true, false), (Mark::Tick, false));
        assert_eq!(shell(false, false, false), (Mark::Prompt, false));
        assert_eq!(shell(true, true, false), (Mark::Running, true));
        // Exited is exited, whatever it last ran or left unseen.
        assert_eq!(shell(false, true, true), (Mark::Exited, false));
        assert_eq!(shell(true, false, true), (Mark::Exited, false));
    }

    /// A shell at its prompt is the small filled square, as idle Claude is
    /// the small dot, the same square a not-started shell wears in gray and
    /// a running command's breath starts from; an exited shell is that
    /// small square hollow, smaller than the running breath's other end.
    #[test]
    fn a_shell_at_its_prompt_is_the_small_filled_square() {
        assert_eq!(Mark::Prompt.still_square(), Some((SMALL_SQUARE, true)));
        assert_eq!(
            Mark::NotStarted { claude: false }.still_square(),
            Mark::Prompt.still_square()
        );
        assert_eq!(Mark::Exited.still_square(), Some((SMALL_SQUARE, false)));
        for mark in [Mark::Running, Mark::Idle, Mark::NotStarted { claude: true }] {
            assert_eq!(mark.still_square(), None, "{mark:?}");
        }
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let draw = |(side, filled): (f32, bool)| {
                square_of(
                    &painted_at(ppp, |p| square(p, Pos2::ZERO, side, filled, Color32::RED)),
                    ppp,
                )
            };
            let breath = |b: f32| {
                square_of(
                    &painted_at(ppp, |p| square_breath(p, Pos2::ZERO, b, Color32::RED)),
                    ppp,
                )
            };
            assert_eq!(draw(Mark::Prompt.still_square().unwrap()), breath(0.0));
            // Exited: the prompt's square across, but hollow, and smaller
            // than the running breath's top.
            let (exited, prompt) = (
                draw(Mark::Exited.still_square().unwrap()),
                draw(Mark::Prompt.still_square().unwrap()),
            );
            assert_eq!(exited.0, prompt.0, "{ppp}: the prompt's size");
            assert!(exited.1 > 0.0 && prompt.1 == 0.0, "{ppp}: hollow");
            assert!(exited.0 < breath(1.0).0, "{ppp}: not the breath's top");
        }
    }

    /// The (outer side, hole side) of the one square `shapes` draws, in
    /// pixels: a filled square has no hole.
    fn square_of(shapes: &[egui::Shape], ppp: f32) -> (f32, f32) {
        match shapes {
            [egui::Shape::Rect(r)] => {
                let side = r.rect.width() * ppp;
                assert!((side - r.rect.height() * ppp).abs() < 1e-3, "square");
                if r.fill != Color32::TRANSPARENT {
                    assert_eq!(r.stroke.width, 0.0, "filled or stroked, not both");
                    (side, 0.0)
                } else {
                    (side, side - 2.0 * r.stroke.width * ppp)
                }
            }
            _ => panic!("one square, not {shapes:?}"),
        }
    }

    fn painted_at(ppp: f32, paint: impl Fn(&egui::Painter)) -> Vec<egui::Shape> {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        let out = ctx.run_ui(egui::RawInput::default(), |ui| paint(ui.painter()));
        out.shapes.into_iter().map(|c| c.shape).collect()
    }

    /// A shell's running square morphs from its prompt's small filled
    /// square (the not-started marker's size) to the hollow square and back,
    /// never down to a pixel: breathed in it is the small square exactly,
    /// breathed out (and with `rail.animate` off) the hollow one, and in
    /// between its edge and its hole each move a pixel at a time, so it
    /// never jumps from filled to hollow.
    #[test]
    fn the_running_square_morphs_from_the_small_square_to_the_hollow_one() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let at = |breath: f32| {
                square_of(
                    &painted_at(ppp, |p| square_breath(p, Pos2::ZERO, breath, Color32::RED)),
                    ppp,
                )
            };
            let small = square_of(
                &painted_at(ppp, |p| {
                    square(p, Pos2::ZERO, SMALL_SQUARE, true, Color32::RED)
                }),
                ppp,
            );
            let hollow = square_of(
                &painted_at(ppp, |p| {
                    square(p, Pos2::ZERO, SQUARE_SIDE, false, Color32::RED)
                }),
                ppp,
            );
            assert_eq!(at(0.0), small, "breathed in: the prompt's square");
            assert!(small.0 >= (SMALL_SQUARE * ppp).round() && small.1 == 0.0);
            assert_eq!(at(1.0), hollow, "breathed out: the hollow square");
            assert!(hollow.1 > 0.0, "hollow");
            let steps = 400;
            let mut last = at(0.0);
            for k in 1..=steps {
                let now = at(k as f32 / steps as f32);
                assert!(now.0 >= last.0, "{ppp}: {last:?} -> {now:?}");
                assert!(
                    now.0 - last.0 <= 1.0 + 1e-3,
                    "{ppp}: edge jumped {last:?} -> {now:?}"
                );
                // The hole opens from nothing: it never goes from none to
                // more than a couple of pixels, nor back.
                assert!(
                    (now.1 - last.1).abs() <= 2.0 + 1e-3,
                    "{ppp}: hole jumped {last:?} -> {now:?}"
                );
                last = now;
            }
        }
    }

    /// The one mesh [`annulus`] draws, which must be all that was drawn.
    fn the_mesh(shapes: &[egui::Shape]) -> &egui::Mesh {
        match shapes {
            [egui::Shape::Mesh(m)] => m,
            _ => panic!("one mesh, not {shapes:?}"),
        }
    }

    /// The ink a mesh lays at `at`, 0 to 1 of `full`'s alpha: what its
    /// triangles' colours blend to there, as the GPU draws them.
    fn ink_at(mesh: &egui::Mesh, at: Pos2, full: Color32) -> f32 {
        let mut ink = 0.0f32;
        for t in mesh.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| &mesh.vertices[t[k] as usize]);
            let (v0, v1, v2) = (b.pos - a.pos, c.pos - a.pos, at - a.pos);
            let det = v0.x * v1.y - v1.x * v0.y;
            if det.abs() < 1e-12 {
                continue;
            }
            let u = (v2.x * v1.y - v1.x * v2.y) / det;
            let v = (v0.x * v2.y - v2.x * v0.y) / det;
            if u < -1e-6 || v < -1e-6 || u + v > 1.0 + 1e-6 {
                continue;
            }
            let alpha = |x: &egui::epaint::Vertex| x.color.a() as f32;
            let here = alpha(a) * (1.0 - u - v) + alpha(b) * u + alpha(c) * v;
            ink = ink.max(here / full.a() as f32);
        }
        ink
    }

    /// The ink of a calm mark's one mesh along the ray through each ring's
    /// first vertex, (radius, 0 to 1 of `full`'s alpha) from the centre
    /// out: between two points it ramps straight, as the mesh draws it.
    fn ray(shapes: &[egui::Shape], full: Color32) -> Vec<(f32, f32)> {
        let mesh = the_mesh(shapes);
        // The rings are round the centre, so it is all their vertices' mean.
        let sum = mesh
            .vertices
            .iter()
            .fold(Vec2::ZERO, |s, v| s + v.pos.to_vec2());
        let centre = (sum / mesh.vertices.len() as f32).to_pos2();
        let mut ray: Vec<(f32, f32)> = mesh
            .vertices
            .iter()
            .map(|v| (v.pos - centre, v.color.a() as f32 / full.a() as f32))
            .filter(|(d, _)| d.y.abs() < 1e-4 && d.x >= -1e-4)
            .map(|(d, a)| (d.x.max(0.0), a))
            .collect();
        ray.sort_by(|a, b| a.0.total_cmp(&b.0));
        ray
    }

    /// The disc between `outer` and `inner` a calm mark's one mesh draws,
    /// in points: how big a disc its ink makes all told, and how big a hole
    /// it leaves in the middle, both by area, so a hole under a pixel that
    /// only thins the centre counts as that much hole.
    fn band(shapes: &[egui::Shape]) -> (f32, f32) {
        let ray = ray(shapes, Color32::RED);
        // ∫ 2r·a(r) dr over one straight piece, or of the ink missing,
        // 1 - a(r); Simpson's rule is exact for it.
        let piece = |(r0, a0): (f32, f32), (r1, a1): (f32, f32), ink: bool| {
            let f = |r: f32, a: f32| 2.0 * r * if ink { a } else { 1.0 - a };
            let (rm, am) = ((r0 + r1) / 2.0, (a0 + a1) / 2.0);
            (r1 - r0) / 6.0 * (f(r0, a0) + 4.0 * f(rm, am) + f(r1, a1))
        };
        let peak = ray
            .iter()
            .position(|&(_, a)| a >= 1.0 - 1e-3)
            .expect("full ink somewhere");
        // The hole: all that is missing inside the full band; the disc: the
        // band's ink and all inside it.
        let mut hole = ray[0].0 * ray[0].0;
        let mut disc = ray[peak].0 * ray[peak].0;
        for w in ray.windows(2).take(peak) {
            hole += piece(w[0], w[1], false);
        }
        for w in ray.windows(2).skip(peak) {
            disc += piece(w[0], w[1], true);
        }
        (disc.sqrt(), hole.max(0.0).sqrt())
    }

    fn painted(paint: impl Fn(&egui::Painter)) -> Vec<egui::Shape> {
        let ctx = egui::Context::default();
        let out = ctx.run_ui(egui::RawInput::default(), |ui| paint(ui.painter()));
        out.shapes.into_iter().map(|c| c.shape).collect()
    }

    /// Claude's calm busy mark morphs from the idle dot to the open ring
    /// and back: breathed in it is the idle dot, breathed out the ring, and
    /// all the way round the breath, at any scale, both edges move a little
    /// at a time, the hole fading in from nothing, so there is no jump where
    /// a filled disc turns into a stroked ring at the end of the breath.
    #[test]
    fn the_calm_mark_morphs_from_the_idle_dot_to_the_open_ring() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let at = |breath: f32| {
                band(&painted_at(ppp, |p| {
                    calm_ring(p, Pos2::ZERO, breath, Color32::RED)
                }))
            };
            // An edge anti-aliased across a pixel reads a hair bigger by
            // area: (r² + px²/12)^½.
            let near = |got: f32, r: f32| (got - r).abs() < 0.05;
            let (outer, inner) = at(0.0);
            assert!(
                near(outer, DOT_R),
                "{ppp}: breathed in, the idle dot: {outer}"
            );
            assert_eq!(inner, 0.0, "{ppp}: breathed in, no hole at all");
            let (outer, inner) = at(1.0);
            assert!(near(outer, RING_R + RING_STROKE), "{ppp}: {outer}");
            assert!(near(inner, RING_R), "{ppp}: {inner}");
            // One whole breath, out and back in, in fine steps of time.
            let steps = 600;
            let breath = |k: usize| calm_breath(Some(CALM_PERIOD * k as f64 / steps as f64));
            let mut last = at(breath(0));
            for k in 1..=steps {
                let now = at(breath(k));
                let out = k <= steps / 2;
                assert!(
                    // The fill's alpha is a byte: allow its rounding.
                    if out {
                        now.0 >= last.0 - 1e-3 && now.1 >= last.1 - 1e-3
                    } else {
                        now.0 <= last.0 + 1e-3 && now.1 <= last.1 + 1e-3
                    },
                    "{ppp}: {last:?} -> {now:?}"
                );
                assert!(
                    (now.0 - last.0).abs() < 0.05,
                    "{ppp}: outer edge jumped {last:?} -> {now:?}"
                );
                // The hole opens and closes by degrees, never a pixel at once.
                assert!(
                    (now.1 - last.1).abs() < 0.05,
                    "{ppp}: hole jumped {last:?} -> {now:?}"
                );
                last = now;
            }
            assert_eq!(last, at(0.0), "{ppp}: back to the idle dot");
        }
    }

    /// Nothing is ever drawn inside the calm mark's hole (a disc filling a
    /// hole too small to draw would show as a dot inside the ring just before
    /// it closed). At every frame of a breath, at the animation clock's pace
    /// and in fine steps between, at 1x, 1.25x and 1.5x: the mark is one
    /// shape; where the hole is wider than its anti-aliased edge no ink falls
    /// inside it; and from the centre out the ink only rises to the full band
    /// and then only falls, so even a hole under a pixel is a thinning
    /// centre, never a dot in a ring.
    #[test]
    fn the_calm_mark_draws_nothing_inside_its_hole() {
        let paced = (0..=12).map(|k| k as f64 * 0.25);
        let fine = (0..=240).map(|k| CALM_PERIOD * k as f64 / 240.0);
        for ppp in [1.0, 1.25, 1.5] {
            let px = 1.0 / ppp;
            for t in paced.clone().chain(fine.clone()) {
                let breath = calm_breath(Some(t));
                let shapes = painted_at(ppp, |p| calm_ring(p, Pos2::ZERO, breath, Color32::RED));
                let mesh = the_mesh(&shapes);
                // Clear of the hole's anti-aliased edge.
                let hole = RING_R * breath - px / 2.0;
                if hole > 0.0 {
                    for part in [0.0, 0.3, 0.6, 0.9, 0.99] {
                        for k in 0..7 {
                            let turn = std::f32::consts::TAU * (k as f32 + 0.5) / 7.0;
                            let at = Pos2::ZERO + hole * part * Vec2::angled(turn);
                            let ink = ink_at(mesh, at, Color32::RED);
                            assert_eq!(ink, 0.0, "{ppp} at {t}s: ink {ink} at {at:?}, hole {hole}");
                        }
                    }
                }
                let ray = ray(&shapes, Color32::RED);
                let peak = ray.iter().position(|&(_, a)| a >= 1.0 - 1e-3).unwrap();
                for w in ray.windows(2) {
                    let rising = w[1].0 <= ray[peak].0;
                    assert!(
                        if rising {
                            w[1].1 >= w[0].1
                        } else {
                            w[1].1 <= w[0].1
                        },
                        "{ppp} at {t}s: a bump in the ink {ray:?}"
                    );
                }
            }
        }
    }

    /// A header counts a tab showing the wave as working, so its breathing
    /// dot and count stay while only the tab's subagents or background shell
    /// run, and a plain shell running a command; the tick, the idle dot and a
    /// shell at its prompt are not working.
    #[test]
    fn a_header_counts_a_waving_tab_as_working() {
        let humming = |state| RowData {
            background: true,
            ..row(state)
        };
        assert_eq!(tallies(&[humming(ClaudeState::Idle)]), (1, 1, 0));
        // Unseen with its work still running is the wave too.
        assert_eq!(tallies(&[humming(ClaudeState::DoneUnseen)]), (1, 1, 0));
        assert_eq!(
            tallies(&[
                row(ClaudeState::Busy),
                humming(ClaudeState::Idle),
                row(ClaudeState::DoneUnseen),
                row(ClaudeState::Idle),
                humming(ClaudeState::NeedsYou),
            ]),
            (5, 2, 1),
            "busy and wave count; tick, idle dot and flag do not"
        );
        let shell = |running, exited| RowData {
            running,
            exited,
            ..row(ClaudeState::None)
        };
        assert_eq!(
            tallies(&[shell(true, false), shell(false, false), shell(true, true)]),
            (3, 1, 0),
            "a shell running a command works; at its prompt or exited it does not"
        );
        // A tab not started has nothing running, whatever state lingers.
        let pending = RowData {
            started: false,
            ..humming(ClaudeState::Busy)
        };
        assert_eq!(tallies(&[pending]), (1, 0, 0));
    }

    /// A restored tab that has not spawned is told by what it comes back
    /// as, whatever Claude state lingers for it.
    #[test]
    fn a_tab_not_started_is_a_circle_if_it_resumes_claude_else_a_square() {
        let pending = |resumes_claude, claude| RowData {
            started: false,
            resumes_claude,
            ..row(claude)
        };
        assert_eq!(
            Mark::of(&pending(false, ClaudeState::None)),
            Mark::NotStarted { claude: false }
        );
        assert_eq!(
            Mark::of(&pending(true, ClaudeState::None)),
            Mark::NotStarted { claude: true }
        );
        assert_eq!(
            Mark::of(&pending(true, ClaudeState::Busy)),
            Mark::NotStarted { claude: true },
            "nothing is running, so nothing is busy"
        );
    }

    /// Every mark is in the category's colour, Claude and shell alike; only
    /// a tab not started yet, or a shell that has exited, is gray.
    #[test]
    fn every_mark_wears_the_category_colour_but_not_started_is_gray() {
        let (cat, dim) = (Color32::from_rgb(200, 120, 40), Color32::from_gray(90));
        for mark in [
            Mark::Busy,
            Mark::Flag,
            Mark::Tick,
            Mark::Idle,
            Mark::Wave,
            Mark::Running,
            Mark::Prompt,
        ] {
            assert_eq!(mark.color(cat, dim), cat, "{mark:?}");
        }
        for claude in [false, true] {
            assert_eq!(Mark::NotStarted { claude }.color(cat, dim), dim);
        }
        assert_eq!(Mark::Exited.color(cat, dim), dim, "an exited shell is gray");
    }

    /// A shell's square lands on whole device pixels at any scale and any
    /// fractional position, and is as many pixels across as asked: a square
    /// straddling a pixel edge blurs at this size.
    #[test]
    fn a_shell_square_sits_on_whole_pixels() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            for filled in [true, false] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(ppp);
                let mut out = None;
                for _ in 0..2 {
                    let shapes = ctx
                        .run_ui(egui::RawInput::default(), |ui| {
                            square(
                                ui.painter(),
                                Pos2::new(32.3, 13.7),
                                6.0,
                                filled,
                                Color32::RED,
                            );
                        })
                        .shapes;
                    out = shapes.into_iter().find_map(|c| match c.shape {
                        egui::Shape::Rect(r) => Some(r.rect),
                        _ => None,
                    });
                }
                let r = out.expect("a square was drawn");
                let whole = |v: f32| ((v * ppp) - (v * ppp).round()).abs() < 1e-3;
                assert!(
                    whole(r.min.x) && whole(r.min.y) && whole(r.max.x) && whole(r.max.y),
                    "{ppp}: {r:?}"
                );
                assert!(((r.width() * ppp) - (6.0 * ppp).round()).abs() < 1e-3);
                assert!((r.width() - r.height()).abs() < 1e-3, "{r:?}");
            }
        }
    }
}
