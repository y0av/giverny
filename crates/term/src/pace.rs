//! How soon terminal output gets a frame.
//!
//! Every frame repaints the whole window, and on a software renderer (WSLg's
//! llvmpipe, where Giverny runs today) that is real CPU: ~12% of a core per
//! frame a second at 1280×820 (#43). A working Claude session redraws its
//! spinner line ~9 times a second whether anyone is looking or not, so
//! drawing every burst the moment it lands cost ~150% of a core for as long
//! as it worked.
//!
//! So output is paced by whether somebody is using the window. Within
//! [`ATTENDED`] of input it is drawn at once — typing, and what the program
//! prints in answer to it, must never wait. Otherwise it waits for the next
//! tick of a shared clock, [`STEP_FOCUSED`] apart while the window has focus
//! and [`STEP_BACKGROUND`] while it does not: a stream still reads as a
//! stream, a spinner still turns, and the frames nobody was going to see are
//! simply not drawn.
//!
//! The same clock drives the app's own glyph animations (the rail's
//! spinners), so a working tab on screen and a spinner in the rail share
//! their frames instead of interleaving two cadences into twice as many.
//!
//! Output from a tab that is not on screen asks for nothing at all past its
//! first byte: the session's dirty flag stays set until the widget draws it,
//! and only its false→true edge wakes the window (see `proxy`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long after input the window counts as in use.
pub const ATTENDED: Duration = Duration::from_millis(1500);
/// The clock's tick while the window has focus: four frames a second.
pub const STEP_FOCUSED: Duration = Duration::from_millis(250);
/// Its tick while the window is in the background. A whole multiple of
/// [`STEP_FOCUSED`], so both land on the same grid.
pub const STEP_BACKGROUND: Duration = Duration::from_millis(500);

/// A spinner's frames per [`STEP_FOCUSED`] tick while the window draws on a
/// GPU: twelve a second, which reads as turning rather than ticking (#70).
///
/// [`STEP_FOCUSED`] is what a frame costs on a software renderer. On a GPU
/// (WSLg's d3d12 driver, since #59) a frame is well under 1% of a core, so a
/// spinner that has to hop a quarter second at a time saves almost nothing,
/// and it looks broken: slow, and uneven, because its snapped clock turns
/// only when a frame happens to land past the tick.
///
/// A whole number of frames per tick, so the spinner's frames fall on the
/// shared clock's grid and every frame drawn for the terminal or the
/// heartbeat is one of them rather than an extra one in between.
pub const SPIN_PER_STEP: u64 = 3;
/// [`SPIN_PER_STEP`] while the window is in the background: four a second,
/// still even, at a third of the cost.
pub const SPIN_PER_STEP_BACKGROUND: u64 = 1;

/// Frames are cheap: the window draws on a GPU. Set once the renderer is
/// known; until then, and on a software rasterizer, animations keep to the
/// shared [`STEP_FOCUSED`] clock.
static CHEAP_FRAMES: AtomicBool = AtomicBool::new(false);

/// Say whether the window draws on a GPU (see [`CHEAP_FRAMES`]).
pub fn set_cheap_frames(cheap: bool) {
    CHEAP_FRAMES.store(cheap, Ordering::Relaxed);
}

/// Does the window draw on a GPU?
pub fn cheap_frames() -> bool {
    CHEAP_FRAMES.load(Ordering::Relaxed)
}

/// How long until a smooth animation's next frame: the next of
/// [`SPIN_PER_STEP`] (or [`SPIN_PER_STEP_BACKGROUND`]) even slots in a
/// [`STEP_FOCUSED`] tick, aimed [`LAND_PAST`] beyond it as [`until_next_tick`]
/// is. The angle is read from the real clock, so a frame that lands late
/// shows where the spinner is, not where it was.
pub fn until_next_spin(focused: bool, predicted_dt: f32) -> Duration {
    let per_step = if focused {
        SPIN_PER_STEP
    } else {
        SPIN_PER_STEP_BACKGROUND
    };
    to_slot(
        epoch().elapsed().as_micros() as u64 + 1000,
        per_step,
        dt_ms(predicted_dt),
    )
}

/// [`until_next_spin`] over plain numbers: `now` in microseconds on the
/// [`now_ms`] clock, `dt` in milliseconds.
fn to_slot(now_us: u64, per_step: u64, dt: u64) -> Duration {
    let step = STEP_FOCUSED.as_micros() as u64;
    // Slot n starts at n * step / per_step, rounded up to the microsecond:
    // no drift against the tick grid however long the spinner runs.
    let next = ((now_us * per_step / step + 1) * step).div_ceil(per_step);
    Duration::from_micros(next - now_us) + Duration::from_millis(dt + LAND_PAST)
}

/// Milliseconds since [`epoch`], plus one so that zero means "never".
static LAST_INPUT: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME: AtomicU64 = AtomicU64::new(0);
static FOCUSED: AtomicBool = AtomicBool::new(true);
/// egui's predicted frame time, in milliseconds (see [`LAND_PAST`]).
static PREDICTED_DT: AtomicU64 = AtomicU64::new(17);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64 + 1
}

/// Somebody touched the window: a key, text, the pointer.
pub fn note_input() {
    LAST_INPUT.store(now_ms(), Ordering::Relaxed);
}

/// A terminal was just drawn, in a window that does or does not have focus,
/// by an egui predicting `predicted_dt` seconds per frame.
pub fn note_frame(focused: bool, predicted_dt: f32) {
    LAST_FRAME.store(now_ms(), Ordering::Relaxed);
    FOCUSED.store(focused, Ordering::Relaxed);
    PREDICTED_DT.store(dt_ms(predicted_dt), Ordering::Relaxed);
}

/// Whether any of the app's windows has focus, said once a frame after all
/// of them are drawn: each terminal's [`note_frame`] only knows its own
/// window, and with several the last one drawn would decide for all.
pub fn note_focus(focused: bool) {
    FOCUSED.store(focused, Ordering::Relaxed);
}

fn dt_ms(predicted_dt: f32) -> u64 {
    // Bounded: a wild prediction must not stall the clock.
    (predicted_dt.max(0.0) * 1000.0).ceil().min(200.0) as u64
}

/// How long output arriving now should wait for its frame.
pub fn output_delay() -> Duration {
    delay(
        now_ms(),
        LAST_INPUT.load(Ordering::Relaxed),
        LAST_FRAME.load(Ordering::Relaxed),
        FOCUSED.load(Ordering::Relaxed),
        PREDICTED_DT.load(Ordering::Relaxed),
    )
}

/// How long until the clock's next tick, for an animation that wants to move
/// on it. Aims [`LAND_PAST`] beyond the tick, so the frame it asks for sees
/// the new step.
pub fn until_next_tick(focused: bool, predicted_dt: f32) -> Duration {
    to_tick(now_ms(), focused, dt_ms(predicted_dt))
}

/// The clock, in seconds, snapped down to its last [`STEP_FOCUSED`] tick:
/// what an animation draws from, so every glyph in a frame is on the same
/// step and a frame drawn for some other reason does not nudge them.
pub fn anim_time() -> f64 {
    let step = STEP_FOCUSED.as_millis() as u64;
    ((now_ms() / step) * step) as f64 / 1000.0
}

/// The rule, over plain numbers (milliseconds; zero means never).
fn delay(now: u64, last_input: u64, last_frame: u64, focused: bool, dt: u64) -> Duration {
    if last_input != 0 && now.saturating_sub(last_input) < ATTENDED.as_millis() as u64 {
        return Duration::ZERO;
    }
    // Quiet for a whole step: nothing is being paced, draw it now.
    let step = step(focused).as_millis() as u64;
    if last_frame == 0 || now.saturating_sub(last_frame) >= step {
        return Duration::ZERO;
    }
    to_tick(now, focused, dt)
}

fn step(focused: bool) -> Duration {
    if focused {
        STEP_FOCUSED
    } else {
        STEP_BACKGROUND
    }
}

/// How far past a tick a paced frame aims, beyond egui's predicted frame
/// time.
///
/// egui brings every delayed repaint forward by its predicted frame time,
/// "to avoid overshooting". Aimed at the tick itself, the frame then lands
/// *early*, sees the old step, asks for the tick again — now only
/// milliseconds off, so at once — and repeats until the clock crosses: ~20
/// frames a second, all drawing the same picture, and with a fixed 40 ms
/// margin still two a tick (measured). So the request adds the prediction
/// back, and then a little more.
const LAND_PAST: u64 = 15;

fn to_tick(now: u64, focused: bool, dt: u64) -> Duration {
    let step = step(focused).as_millis() as u64;
    Duration::from_millis(step - now % step + dt + LAND_PAST)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn predictions_are_bounded() {
        assert_eq!(dt_ms(1.0 / 60.0), 17);
        assert_eq!(dt_ms(-1.0), 0);
        assert_eq!(dt_ms(10.0), 200);
    }

    #[test]
    fn typing_is_never_held_back() {
        // Input 100 ms ago, a frame 1 ms ago: the echo still goes at once.
        assert_eq!(delay(10_000, 9_900, 9_999, true, 25), Duration::ZERO);
        assert_eq!(delay(10_000, 9_900, 9_999, false, 25), Duration::ZERO);
    }

    #[test]
    fn unattended_output_waits_for_the_next_tick() {
        // Nobody has touched it for a minute; the last frame was 50 ms ago.
        assert_eq!(delay(70_050, 10_000, 70_000, true, 25), MS(240));
        assert_eq!(delay(70_050, 10_000, 70_000, false, 25), MS(490));
        // Never touched at all counts the same.
        assert_eq!(delay(70_050, 0, 70_000, true, 25), MS(240));
    }

    #[test]
    fn a_quiet_terminal_draws_its_first_output_at_once() {
        assert_eq!(delay(70_000, 10_000, 60_000, true, 25), Duration::ZERO);
        assert_eq!(delay(70_000, 10_000, 0, false, 25), Duration::ZERO);
    }

    #[test]
    fn attention_lapses_after_its_window() {
        let lapsed = 10_000 + ATTENDED.as_millis() as u64;
        assert_eq!(delay(lapsed + 10, 10_000, lapsed, true, 25), MS(280));
    }

    #[test]
    fn ticks_share_one_grid() {
        // Focused and background ticks coincide wherever both fall.
        assert_eq!(to_tick(1_000, true, 25), MS(290));
        assert_eq!(to_tick(1_000, false, 25), MS(540));
        assert_eq!(to_tick(1_249, true, 25), MS(41));
        assert_eq!(to_tick(1_249, false, 25), MS(291));
    }

    #[test]
    fn spinner_slots_share_the_tick_grid() {
        // Where the frame asked for at `now` is aimed, minus the margin.
        let slot =
            |now: u64, per_step| now + to_slot(now, per_step, 25).as_micros() as u64 - 40_000;
        // Three slots a tick, the third ending on the next tick exactly.
        assert_eq!(slot(1_000_000, 3), 1_083_334);
        assert_eq!(slot(1_083_334, 3), 1_166_667);
        assert_eq!(slot(1_166_667, 3), 1_250_000);
        assert_eq!(slot(1_249_999, 3), 1_250_000);
        // Still on the grid an hour in.
        assert_eq!(slot(3_600_000_000 - 1, 3), 3_600_000_000);
        // In the background, one a tick: the tick itself.
        assert_eq!(to_slot(1_000_000, 1, 25), to_tick(1_000, true, 25));
    }

    #[test]
    fn a_frame_that_lands_on_its_tick_asks_for_the_next_one() {
        // egui brings the frame forward by its predicted frame time, which
        // the request added back: it lands past the tick, and the next
        // request is most of a step away, not a retry.
        for dt in [17, 60, 120] {
            let landed = 1_000 + to_tick(1_000, true, dt).as_millis() as u64 - dt;
            assert!(landed > 1_250, "dt {dt}: landed at {landed}");
            assert!(to_tick(landed, true, dt) > MS(200), "dt {dt}");
        }
    }
}
