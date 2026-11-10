//! One worker's transcript as a queue of tasks: the tokens
//! each of its turns added.
//!
//! A worker handed a second task (`start <task> --agent <worker>`) holds one row per task in
//! the management panel. Each finished task's row shows what *that task* spent —
//! the turns between its hand-off and the next one — never the worker's
//! whole count, which would be counted once per row. That needs every turn
//! with its time, which is what [`WorkerLog`] keeps, following the
//! transcript as it grows.
//!
//! A turn's tokens are what it **added**: its output plus the input read
//! fresh (uncached input and cache creation). Cache reads are the context
//! carried over from earlier turns and are left out, so the sum over a span
//! never counts a turn twice and never goes negative across a compaction.
//! This is a different number from the live count Claude Code's agents view
//! shows (context plus output), which is what a Running row keeps.
//!
//! The dispatcher's messages are read for nothing: whatever they say, no
//! message hands a row to a worker.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// One billed turn: when it was written and what it added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub at_ms: u64,
    pub added: u64,
}

/// What one `usage` object added: output, uncached input and cache creation.
pub fn added_tokens(u: &Value) -> u64 {
    let n = |k: &str| u.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64;
    n("output_tokens") + n("input_tokens") + n("cache_creation_input_tokens")
}

/// Follows one worker's whole transcript: the first [`poll`] reads it from
/// the start, every later one only what was appended.
///
/// [`poll`]: WorkerLog::poll
#[derive(Debug, Clone, Default)]
pub struct WorkerLog {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    turns: Vec<Turn>,
    /// Message id → its turn: Claude Code writes one line per content
    /// block of a reply, each repeating the reply's usage.
    by_id: HashMap<String, usize>,
}

impl WorkerLog {
    pub fn new(path: impl Into<PathBuf>) -> WorkerLog {
        WorkerLog {
            path: path.into(),
            ..WorkerLog::default()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// When the worker first wrote: its first turn.
    pub fn first_ms(&self) -> Option<u64> {
        self.turns.iter().map(|t| t.at_ms).min()
    }

    /// The tokens the turns in `[from_ms, to_ms)` added (`to_ms` `None`:
    /// every turn from `from_ms` on).
    pub fn added(&self, from_ms: u64, to_ms: Option<u64>) -> u64 {
        self.turns
            .iter()
            .filter(|t| t.at_ms >= from_ms && to_ms.is_none_or(|to| t.at_ms < to))
            .map(|t| t.added)
            .sum()
    }

    /// Read what was appended. True when anything was. A file that shrank
    /// (rewritten) is read again from the start.
    pub fn poll(&mut self) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return false;
        };
        if len < self.offset {
            *self = WorkerLog::new(std::mem::take(&mut self.path));
        }
        if len == self.offset {
            return false;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        if file
            .take(len - self.offset)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return false;
        }
        self.offset += bytes.len() as u64;
        let mut chunk = std::mem::take(&mut self.partial);
        chunk.extend_from_slice(&bytes);
        let cut = chunk.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in chunk[..cut].split(|&b| b == b'\n') {
            self.fold(line);
        }
        self.partial = chunk[cut..].to_vec();
        true
    }

    fn fold(&mut self, line: &[u8]) {
        if find(line, b"\"usage\"").is_none() {
            return;
        }
        let Some((at_ms, id, added)) = parsed_turn(line) else {
            return;
        };
        self.push(at_ms, id, added);
    }

    fn push(&mut self, at_ms: u64, id: Option<String>, added: u64) {
        match id.as_ref().and_then(|id| self.by_id.get(id)) {
            // The same reply again: its usage is the same or later.
            Some(&i) => self.turns[i].added = self.turns[i].added.max(added),
            None => {
                if let Some(id) = id {
                    self.by_id.insert(id, self.turns.len());
                }
                self.turns.push(Turn { at_ms, added });
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// When a transcript's worker first wrote: the first `timestamp` among its
/// opening lines, read without the rest of the file.
pub fn first_written_ms(path: &Path) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(file)
        .lines()
        .take(8)
        .map_while(Result::ok)
        .find_map(|l| {
            let v: Value = serde_json::from_str(&l).ok()?;
            v.get("timestamp").and_then(Value::as_str).and_then(ms_of)
        })
}

fn ms_of(s: &str) -> Option<u64> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond().max(0) as u64)
}

/// A billed turn, from an assistant line: when, its message id, and what
/// it added.
fn parsed_turn(line: &[u8]) -> Option<(u64, Option<String>, u64)> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    if msg.get("model").and_then(Value::as_str) == Some("<synthetic>") {
        return None;
    }
    let at_ms = ms_of(v.get("timestamp").and_then(Value::as_str)?)?;
    let id = msg.get("id").and_then(Value::as_str).map(str::to_string);
    Some((at_ms, id, added_tokens(msg.get("usage")?)))
}
