//! Ending background jobs from the rail.
//!
//! A background job is Claude Code's, not a tab's: closing the tab opened on
//! one leaves it running, and BACKGROUND keeps listing it. Ending one is a
//! separate, explicit act — `claude stop <id>` or `claude rm <id>` — run off
//! the UI thread, its row saying so meanwhile.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use giverny_claude::jobs::End;

/// Where ending one job has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The CLI is running: the row says so and offers nothing more.
    Running(End),
    /// It answered yes. The row stays hidden until the jobs scan stops
    /// listing the job, so it does not flash back for a scan or two.
    Done(Instant),
    /// It answered no: the row is back, with what it said.
    Failed(End, String),
}

/// How long a row stays hidden after the CLI said yes, at least (the next
/// jobs scan may still list it), and at most (if the scan never drops it,
/// the job is still there, and the row says so).
const HOLD_MIN: Duration = Duration::from_secs(5);
const HOLD_MAX: Duration = Duration::from_secs(60);

pub struct JobEnds {
    state: HashMap<String, Ending>,
    tx: crossbeam_channel::Sender<(String, Result<(), String>)>,
    rx: crossbeam_channel::Receiver<(String, Result<(), String>)>,
}

impl Default for JobEnds {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        JobEnds {
            state: HashMap::new(),
            tx,
            rx,
        }
    }
}

impl JobEnds {
    /// End job `id`, of the account at `config_dir`, on a thread of its own.
    /// One at a time per job: a second click while the first runs is
    /// ignored.
    pub fn start(&mut self, ctx: &egui::Context, id: String, config_dir: PathBuf, how: End) {
        if matches!(self.state.get(&id), Some(Ending::Running(_))) {
            return;
        }
        tracing::info!("job {id}: {} ({})", how.verb(), config_dir.display());
        self.state.insert(id.clone(), Ending::Running(how));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = giverny_claude::jobs::end(&config_dir, &id, how).map_err(|e| {
                tracing::warn!("job {id}: {} failed: {e:#}", how.verb());
                format!("{e:#}")
            });
            let _ = tx.send((id, result));
            ctx.request_repaint();
        });
    }

    /// Fold in what the CLI answered, and let go of rows the jobs scan
    /// has caught up with. `listed`: is the job still in BACKGROUND's list?
    pub fn poll(&mut self, listed: impl Fn(&str) -> bool) {
        while let Ok((id, result)) = self.rx.try_recv() {
            let how = match self.state.get(&id) {
                Some(Ending::Running(how)) => *how,
                _ => End::Stop,
            };
            let next = match result {
                Ok(()) => Ending::Done(Instant::now()),
                Err(said) => Ending::Failed(how, said),
            };
            self.state.insert(id, next);
        }
        self.state.retain(|id, ending| match ending {
            Ending::Done(at) => {
                let held = at.elapsed();
                held < HOLD_MIN || (held < HOLD_MAX && listed(id))
            }
            // A failure is shown for as long as the job is; one that went
            // anyway is nothing to report.
            Ending::Failed(..) => listed(id),
            Ending::Running(_) => true,
        });
    }

    pub fn get(&self, id: &str) -> Option<&Ending> {
        self.state.get(id)
    }

    /// Ended: its row is not drawn.
    pub fn hides(&self, id: &str) -> bool {
        matches!(self.state.get(id), Some(Ending::Done(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_job_stays_hidden_until_the_scan_drops_it() {
        let mut ends = JobEnds::default();
        ends.state
            .insert("6e7e56e0".into(), Ending::Running(End::Stop));
        ends.tx.send(("6e7e56e0".into(), Ok(()))).unwrap();
        ends.poll(|_| true);
        assert!(ends.hides("6e7e56e0"));
        // Still listed, within the hold: still hidden.
        ends.poll(|_| true);
        assert!(ends.hides("6e7e56e0"));
        // Held long enough and no longer listed: forgotten.
        ends.state.insert(
            "6e7e56e0".into(),
            Ending::Done(Instant::now() - HOLD_MIN - Duration::from_secs(1)),
        );
        ends.poll(|_| false);
        assert!(ends.get("6e7e56e0").is_none());
        // Listed past the longest hold: the job is still there, and so is
        // its row.
        ends.state.insert(
            "34c55b2c".into(),
            Ending::Done(Instant::now() - HOLD_MAX - Duration::from_secs(1)),
        );
        ends.poll(|_| true);
        assert!(!ends.hides("34c55b2c"));
    }

    #[test]
    fn a_failure_is_kept_with_what_the_cli_said() {
        let mut ends = JobEnds::default();
        ends.state
            .insert("34c55b2c".into(), Ending::Running(End::Delete));
        ends.tx
            .send(("34c55b2c".into(), Err("no such session".into())))
            .unwrap();
        ends.poll(|_| true);
        assert_eq!(
            ends.get("34c55b2c"),
            Some(&Ending::Failed(End::Delete, "no such session".into()))
        );
        assert!(!ends.hides("34c55b2c"));
        ends.poll(|_| false);
        assert!(ends.get("34c55b2c").is_none(), "gone anyway");
    }
}
