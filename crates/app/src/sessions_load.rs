//! The app's one sampler: what Giverny runs, read every [`EVERY`] (a
//! second) from a thread of its own
//! ([`giverny_claude::use_reading::Sampler`]: one pass over `/proc`, and
//! `nvidia-smi` now and then), so no frame waits on it.
//!
//! Every figure shown comes from the same pass, measured the same way: the
//! sidebar's line is its total (the app and everything under it), its tab
//! rows and category headers its tabs, and the management panel's rows its
//! runs and workers. So a row never shows more than its session, nor a
//! session more than the total. Each frame draws them all from the newest
//! pass ([`latest`]): Claude Code's status line no longer shows a session's
//! figure for them to wait on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use giverny_claude::run_live::{RunLive, TaskLive};
use giverny_claude::use_reading::{Reading, Sampler, Use};

use crate::chrome::Chrome;

/// How often the pass is taken.
const EVERY: Duration = Duration::from_secs(1);

static LAST: OnceLock<Arc<Mutex<Option<Arc<Reading>>>>> = OnceLock::new();

/// Start the sampler (once; later calls do nothing).
pub fn start(ctx: &egui::Context) {
    LAST.get_or_init(|| {
        let last = Arc::new(Mutex::new(None));
        let (l, ctx) = (last.clone(), ctx.clone());
        if let Err(err) = std::thread::Builder::new()
            .name("use-sampler".into())
            .spawn(move || read_loop(&l, &ctx))
        {
            tracing::warn!("use sampler: did not start: {err}");
        }
        last
    });
}

/// The last pass, starting the sampler on the first ask. `None` until its
/// first pass, and where there is nothing to read (no `/proc`).
pub fn latest(ctx: &egui::Context) -> Option<Arc<Reading>> {
    start(ctx);
    LAST.get()?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// A reading's runs, as the management panel keys them.
pub fn task_lives(r: &Reading) -> Vec<TaskLive> {
    r.runs
        .iter()
        .map(|run| TaskLive {
            session: run.session.clone(),
            task: run.task.clone(),
            live: RunLive {
                cpu_pct: run.used.cpu_pct,
                mem_mb: run.used.mem_mb,
                gpu_mb: run.used.gpu_mb,
            },
            agent: run.agent.clone(),
        })
        .collect()
}

/// A reading's workers' use, by agent id, as the management panel shows it.
pub fn workers(r: &Reading) -> HashMap<String, RunLive> {
    r.agents
        .iter()
        .map(|(id, u)| {
            (
                id.clone(),
                RunLive {
                    cpu_pct: u.cpu_pct,
                    mem_mb: u.mem_mb,
                    gpu_mb: u.gpu_mb,
                },
            )
        })
        .collect()
}

fn read_loop(last: &Mutex<Option<Arc<Reading>>>, ctx: &egui::Context) {
    // Each pass also caps every tab's claude.
    let mut sampler = Sampler::capping_tabs();
    let app = std::process::id();
    let runs_dir = giverny_claude::run_live::runs_dir(&giverny_claude::resources::ledger_path(
        &giverny_claude::feed::feed_dir(),
    ));
    // On a steady beat, whatever a pass costs.
    let mut next = std::time::Instant::now();
    loop {
        match sampler.sample(app, &runs_dir) {
            Some(r) => {
                *last.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(r));
                ctx.request_repaint();
            }
            // Nothing to read here and no caps to hold: no figures, no
            // thread. macOS reads nothing either but holds the caps each
            // pass ([`giverny_claude::user_cap`]), so its loop goes on.
            None if cfg!(not(any(target_os = "linux", target_os = "macos"))) => return,
            None => {}
        }
        next += EVERY;
        let now = std::time::Instant::now();
        if next < now {
            next = now;
        }
        std::thread::sleep(next - now);
    }
}

/// The line's figures: `23% CPU  2.0G RAM`, then `  40% GPU` with a GPU
/// that reports per-process utilisation, `  1.2G GPU` (its memory) with
/// one that does not, nothing with none. In the pieces the rail colours one
/// by one, each with its share of the whole machine as
/// [`machine_shares`] takes it of `ram_mb` (GPU compute as it is); the gaps,
/// and GPU memory, whose total is not known here, at none.
///
/// Unpadded: the management panel's use cell pads its figures to fixed
/// widths to hold them in their columns, which a line read from the left
/// has no use for.
pub fn figure_parts(t: &Use, ram_mb: u64) -> Vec<(String, f64)> {
    use giverny_claude::session_use::gb;
    let (cpu, mem) = machine_shares(t, ram_mb);
    let mut parts = vec![
        (format!("{}% CPU", t.cpu_pct.min(100)), cpu),
        ("  ".to_string(), 0.0),
        (format!("{} RAM", gb(t.mem_mb)), mem),
    ];
    match (t.gpu_pct, t.gpu_mb) {
        (Some(p), _) => {
            parts.extend([("  ".to_string(), 0.0), (format!("{p}% GPU"), f64::from(p))])
        }
        (None, Some(g)) => parts.extend([("  ".to_string(), 0.0), (format!("{} GPU", gb(g)), 0.0)]),
        (None, None) => {}
    }
    parts
}

/// The use of the whole machine a tab's (or a group's) figures are
/// coloured by: `(CPU, memory)`, percent. Its CPU figure is
/// already a share of every core; its memory is taken of all the RAM
/// (`ram_mb`), and with no RAM known it is never high.
pub fn machine_shares(u: &Use, ram_mb: u64) -> (f64, f64) {
    let mem = if ram_mb > 0 {
        u.mem_mb as f64 * 100.0 / ram_mb as f64
    } else {
        0.0
    };
    (f64::from(u.cpu_pct), mem)
}

/// How heavy a figure is on the whole machine, as every use figure is
/// coloured — a tab's and the total in the rail, a
/// Running row's in the management panel: light under 20 %,
/// busy from 20 % to 50 %, heavy over it. The share is a
/// [`machine_shares`] figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    Light,
    Busy,
    Heavy,
}

impl Load {
    pub fn of(share: f64) -> Self {
        if share > 50.0 {
            Load::Heavy
        } else if share >= 20.0 {
            Load::Busy
        } else {
            Load::Light
        }
    }

    /// The figure's colour: its own (`light`, dim in the rail, the row's
    /// in the panel) while light, amber when busy, poppy when heavy.
    pub fn color(self, light: egui::Color32, c: Chrome) -> egui::Color32 {
        match self {
            Load::Light => light,
            Load::Busy => c.amber,
            Load::Heavy => c.poppy,
        }
    }
}

/// This machine's total RAM, MiB, read once.
pub fn machine_ram_mb() -> u64 {
    use giverny_core::limits::Machine;
    static RAM: OnceLock<u64> = OnceLock::new();
    *RAM.get_or_init(|| Machine::detect_cpu_ram().ram.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line as read.
    fn figures(t: &Use) -> String {
        figure_parts(t, 0).into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn the_line_is_cpu_ram_and_gpu_when_there_is_one() {
        let mut u = Use {
            cpu_pct: 23,
            mem_mb: 2048,
            gpu_mb: None,
            gpu_pct: None,
        };
        assert_eq!(figures(&u), "23% CPU  2.0G RAM", "no GPU: nothing of it");
        u.gpu_mb = Some(1229);
        assert_eq!(
            figures(&u),
            "23% CPU  2.0G RAM  1.2G GPU",
            "no utilisation: memory"
        );
        u.gpu_pct = Some(40);
        assert_eq!(figures(&u), "23% CPU  2.0G RAM  40% GPU");
    }
}
