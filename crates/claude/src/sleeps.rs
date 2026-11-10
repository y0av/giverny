//! The spans the machine slept through, which come off every task clock.
//!
//! A laptop that sleeps freezes WSL's VM (or, on plain Linux, every
//! process) and its wall clock jumps on waking: a Running row would come
//! back eight hours older. Nothing ran meanwhile, so the pane
//! (`management_panel`) and `giverny manage` take the span off a row's
//! clock as they take off a usage-limit hold.
//!
//! What the span was is not asked of the OS — whether WSL's monotonic clock
//! counts a paused VM differs by host — but seen: Giverny keeps one thread
//! ([`watch`]) that naps [`NAP`] at a time and reads the wall clock on each
//! side. A nap that came back more than [`MIN_SLEEP`] late was a machine
//! that ran nothing, and the span goes as one line, `{"since":…,"until":…}`
//! in epoch ms, onto `sleeps.jsonl` beside the feeds. Append-only like
//! `history.jsonl`; two Giverny windows each write the same sleep, which
//! is why every reader takes the union ([`off_ms`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// The log's file name inside the feed directory. Not `*.json`, so the
/// feed scan ([`crate::feed::find`]) never reads it as a feed.
pub const FILE: &str = "sleeps.jsonl";

/// How long the watcher naps between two looks at the clock.
pub const NAP: Duration = Duration::from_secs(5);

/// How late a nap must come back to be a sleep: far more than a loaded
/// machine ever holds a thread up.
pub const MIN_SLEEP: Duration = Duration::from_secs(60);

/// One span the machine ran nothing, in epoch ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub since: u64,
    pub until: u64,
}

/// `<feed dir>/sleeps.jsonl`.
pub fn path(feed_dir: &Path) -> PathBuf {
    feed_dir.join(FILE)
}

/// Every span that parses, as written.
pub fn load(feed_dir: &Path) -> Vec<Span> {
    let Ok(text) = std::fs::read_to_string(path(feed_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Span>(l).ok())
        .filter(|s| s.until > s.since)
        .collect()
}

/// Append one span: a single `write` of a whole line on a file opened for
/// appending, so two writers interleave lines, never bytes.
pub fn append(feed_dir: &Path, span: Span) -> std::io::Result<()> {
    std::fs::create_dir_all(feed_dir)?;
    let mut line = serde_json::to_vec(&span)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(feed_dir))?
        .write_all(&line)
}

/// The log as the pane reads it: [`load`]ed again only when the file
/// changed, and looked at no more than once a second.
#[derive(Default)]
pub struct Cache {
    spans: Vec<Span>,
    looked: Option<Instant>,
    mtime: Option<SystemTime>,
}

impl Cache {
    /// The spans in `feed_dir`'s log now.
    pub fn poll(&mut self, feed_dir: &Path) -> &[Span] {
        if self
            .looked
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.looked = Some(Instant::now());
            let mtime = std::fs::metadata(path(feed_dir))
                .and_then(|m| m.modified())
                .ok();
            if mtime != self.mtime {
                self.mtime = mtime;
                self.spans = load(feed_dir);
            }
        }
        &self.spans
    }
}

/// The sleep, if any, between two readings of the wall clock taken a nap
/// of `nap` apart. A clock set back is no sleep.
pub fn seen(before_ms: u64, after_ms: u64, nap: Duration) -> Option<Span> {
    let late = after_ms
        .checked_sub(before_ms)?
        .checked_sub(nap.as_millis() as u64)?;
    (late > MIN_SLEEP.as_millis() as u64).then_some(Span {
        since: before_ms,
        until: after_ms,
    })
}

/// Start the watcher: nap, look at the clock, log what was slept through
/// to `feed_dir`. For the life of the process; a second call does nothing.
pub fn watch(feed_dir: PathBuf) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| spawn(feed_dir));
}

fn spawn(feed_dir: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("sleep-watch".into())
        .spawn(move || {
            loop {
                let before = now_ms();
                std::thread::sleep(NAP);
                if let Some(span) = seen(before, now_ms(), NAP)
                    && let Err(e) = append(&feed_dir, span)
                {
                    eprintln!("giverny: {}: {e}", path(&feed_dir).display());
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("giverny: sleep watcher: {e}");
    }
}

/// Milliseconds of `[start, upto]` inside any of `spans`, each counted
/// once however many spans cover it (two windows log the same sleep; a
/// sleep may fall inside a usage-limit hold).
pub fn off_ms(spans: impl IntoIterator<Item = (u64, u64)>, start: u64, upto: u64) -> u64 {
    let mut cut: Vec<(u64, u64)> = spans
        .into_iter()
        .map(|(a, b)| (a.max(start), b.min(upto)))
        .filter(|(a, b)| b > a)
        .collect();
    cut.sort_unstable();
    let mut total = 0;
    let mut reach = start;
    for (a, b) in cut {
        let a = a.max(reach);
        if b > a {
            total += b - a;
            reach = b;
        }
    }
    total
}

/// [`off_ms`] over the sleeps alone.
pub fn slept_ms(sleeps: &[Span], start: u64, upto: u64) -> u64 {
    off_ms(sleeps.iter().map(|s| (s.since, s.until)), start, upto)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    #[test]
    fn a_nap_back_hours_late_is_the_sleep() {
        let s = seen(0, 8 * 60 * MIN, NAP).expect("a sleep");
        assert_eq!((s.since, s.until), (0, 8 * 60 * MIN));
    }
}
