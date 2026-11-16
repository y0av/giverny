//! Token counts for the status line:
//! `session: <n> (+<n>)  ·  subagents: <n>  ·  total: <n>`, the `(+<n>)` being
//! what the session spent before its compactions ([`compacted_tokens`]).
//!
//! The count is the one Claude Code's agents view prints beside a subagent —
//! `latestInputTokens`: input + cache creation + cache read of the **last**
//! API response in a transcript, assigned, never summed. A finished reply is
//! already inside the next request's input, so adding output on top would
//! count it twice. This is the same definition, rule for rule, as coo's
//! `orchestrate-status` (`tokens_of`, `usage_numbers`, `dispatcher_tokens`,
//! `fmt_tokens`; coo#103, coo#118), so the numbers did not move when the
//! segment moved here (giverny#22).
//!
//! Cheap by construction: the session's own count comes off the status line's
//! stdin when Claude Code hands it (`context_window.total_input_tokens`), and
//! a transcript is read **from its tail** — the last usage is near the end, so
//! a chunk is read backwards and doubled only while no usage has turned up.
//! The one thing cached is the compaction scan, which must read the whole
//! transcript: it is kept per transcript and carried forward over what was
//! appended since. Otherwise a render reads a few tens of KB per subagent.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A `usage` object is a few hundred bytes; this is slack.
const USAGE_MAX: usize = 20_000;

/// First tail chunk read off a transcript; doubled until a usage is found.
const TAIL_START: u64 = 64 * 1024;

const SYNTHETIC: &[u8] = b"\"<synthetic>\"";

/// `(input, cache_creation, cache_read)` of one `usage` object.
///
/// Claude Code measures a turn that ran several API iterations by the last
/// real entry of `usage.iterations` (skipping `advisor_message` and
/// `compaction`), falling back to the top level on anything unexpected.
pub fn usage_numbers(u: &Value) -> (u64, u64, u64) {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let base = (
        num(u, "input_tokens") as u64,
        num(u, "cache_creation_input_tokens") as u64,
        num(u, "cache_read_input_tokens") as u64,
    );
    let Some(its) = u.get("iterations").and_then(Value::as_array) else {
        return base;
    };
    if base.0 + base.1 + base.2 == 0 {
        return base;
    }
    let last = its.iter().rfind(|it| {
        it.is_object()
            && !matches!(
                it.get("type").and_then(Value::as_str),
                Some("advisor_message" | "compaction")
            )
    });
    let Some(last) = last else {
        return base;
    };
    if !matches!(
        last.get("type").and_then(Value::as_str),
        Some("message" | "fallback_message")
    ) {
        return base;
    }
    let mut got = [0u64; 3];
    for (slot, k) in got.iter_mut().zip([
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]) {
        match last.get(k).and_then(Value::as_f64) {
            Some(v) if v >= 0.0 => *slot = v as u64,
            _ => return base,
        }
    }
    // The output field must be a number too, as coo checks all four.
    if !last
        .get("output_tokens")
        .and_then(Value::as_f64)
        .is_some_and(|v| v >= 0.0)
    {
        return base;
    }
    if got.iter().sum::<u64>() == 0 {
        return base;
    }
    (got[0], got[1], got[2])
}

/// The context one transcript line sets, if it sets one: an assistant line
/// with a non-synthetic, non-zero `usage`.
fn line_tokens(line: &[u8]) -> Option<u64> {
    let i = find(line, b"\"usage\":")?;
    if find(&line[..i], b"\"assistant\"").is_none() || find(line, SYNTHETIC).is_some() {
        return None;
    }
    let k = i + line[i..].iter().position(|&c| c == b'{')?;
    let mut depth = 0i32;
    let end = line.len().min(k + USAGE_MAX);
    for j in k..end {
        match line[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let u: Value = serde_json::from_slice(&line[k..=j]).ok()?;
                    if !u.is_object() {
                        return None;
                    }
                    let (a, b, c) = usage_numbers(&u);
                    return (a + b + c > 0).then_some(a + b + c);
                }
            }
            _ => {}
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The last context a run of complete lines sets, scanning from the end.
fn last_in(buf: &[u8]) -> Option<u64> {
    buf.split(|&c| c == b'\n').rev().find_map(line_tokens)
}

/// What a transcript says it is carrying now, or `None`: the context of the
/// last API response in it. Never fails — an unreadable file is `None`.
pub fn tokens_of(path: &Path) -> Option<u64> {
    tail_find(path, last_in)
}

/// The transcript's last reply, as far as the prompt cache goes: when it was
/// written, the cache TTL it wrote, and what the next request would re-cache
/// if that TTL has run out — its context plus its own output, which the next
/// request carries too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastReply {
    /// Unix milliseconds.
    pub at_ms: i64,
    /// `None` when the reply wrote nothing to the cache, so its TTL is unknown.
    pub ttl_ms: Option<i64>,
    /// `None` right after a compaction that does not say what it kept.
    pub recache: Option<u64>,
}

/// The last main-conversation reply in a transcript, read from its tail.
///
/// A compaction after it stands in for it: the compaction
/// request was the last to touch the cache, at the boundary's time, and what
/// the next request re-caches is the compacted context
/// ([`after_compaction`]), not the reply's. Its TTL is the reply's.
pub fn last_reply(path: &Path) -> Option<LastReply> {
    let (mut last, compacted) = tail_find(path, |buf| {
        let mut boundary = None;
        for line in buf.split(|&c| c == b'\n').rev() {
            if boundary.is_none()
                && let Some(b) = boundary_at(line)
            {
                boundary = Some(b);
            } else if let Some(reply) = reply_of(line) {
                return Some((reply, boundary));
            }
        }
        None
    })?;
    if let Some((at_ms, post)) = compacted {
        last.at_ms = at_ms;
        last.recache = post.map(|p| p + first_tokens(path).unwrap_or(0));
    }
    Some(last)
}

/// A `compact_boundary` line's time and `postTokens`.
fn boundary_at(line: &[u8]) -> Option<(i64, Option<u64>)> {
    let post = boundary_field(line, "postTokens")?;
    let v: Value = serde_json::from_slice(line).ok()?;
    let at_ms = v
        .get("timestamp")?
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()?
        .as_millisecond();
    Some((at_ms, post))
}

fn reply_of(line: &[u8]) -> Option<LastReply> {
    // Cheap gates before parsing a whole line.
    line_tokens(line)?;
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant")
        || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let at_ms = v
        .get("timestamp")?
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()?
        .as_millisecond();
    let usage = v.get("message")?.get("usage")?;
    let (a, b, c) = usage_numbers(usage);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let wrote = |k: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
    };
    let ttl_ms = if wrote("ephemeral_1h_input_tokens") {
        Some(3_600_000)
    } else if wrote("ephemeral_5m_input_tokens") {
        Some(300_000)
    } else {
        None
    };
    Some(LastReply {
        at_ms,
        ttl_ms,
        recache: Some(a + b + c + output),
    })
}

/// `find` over a transcript's complete lines, reading back from the tail in
/// doubling chunks until it answers or the whole file was read.
fn tail_find<T>(path: &Path, find: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut want = TAIL_START;
    loop {
        let start = len.saturating_sub(want);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        (&mut file).take(len - start).read_to_end(&mut buf).ok()?;
        let whole = start == 0;
        let lines: &[u8] = if whole {
            &buf
        } else {
            // The first line is cut; it is read whole on the next, wider pass.
            match buf.iter().position(|&c| c == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &[],
            }
        };
        if let Some(n) = find(lines) {
            return Some(n);
        }
        if whole {
            return None;
        }
        want = want.saturating_mul(2);
    }
}

/// What one compaction took off the session: `compactMetadata.preTokens` of a
/// `compact_boundary` line — Claude Code's own count of the context it had
/// when it compacted.
///
/// Measured on this machine's four compacted transcripts, 2026-09-28: a
/// compaction stays in the same `.jsonl` under the same `sessionId`, marked by
/// one `{"type":"system","subtype":"compact_boundary","compactMetadata":{…}}`
/// line, and `preTokens` sits 56 to 1,047 tokens above the last assistant
/// usage before it (335,210 against 334,163 in planets `3d917104`) — the
/// compaction request itself. A boundary with no `preTokens` falls back to
/// that last usage.
fn boundary_tokens(line: &[u8]) -> Option<Option<u64>> {
    boundary_field(line, "preTokens")
}

/// A count off a `compact_boundary` line's `compactMetadata`: `None` for any
/// other line, `Some(None)` for a boundary without it.
fn boundary_field(line: &[u8], key: &str) -> Option<Option<u64>> {
    find(line, b"\"compact_boundary\"")?;
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("subtype").and_then(Value::as_str) != Some("compact_boundary") {
        return None;
    }
    Some(
        v.get("compactMetadata")
            .and_then(|m| m.get(key))
            .and_then(Value::as_f64)
            .filter(|n| *n > 0.0)
            .map(|n| n as u64),
    )
}

/// A scan of a transcript's compactions up to `offset`: the tokens every
/// compaction before it took off (`sum`), and the last context set before
/// `offset` (`last`), which a boundary without `preTokens` stands on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CompactScan {
    offset: u64,
    sum: u64,
    last: u64,
}

impl CompactScan {
    /// Carry the scan over `buf`, the bytes from `self.offset` on: whole lines
    /// only, so a line still being written is read next time.
    fn feed(&mut self, buf: &[u8]) {
        let Some(end) = buf.iter().rposition(|&c| c == b'\n') else {
            return;
        };
        for line in buf[..end].split(|&c| c == b'\n') {
            if let Some(pre) = boundary_tokens(line) {
                self.sum += pre.unwrap_or(self.last);
                self.last = 0;
            } else if let Some(n) = line_tokens(line) {
                self.last = n;
            }
        }
        self.offset += end as u64 + 1;
    }

    fn parse(s: &str) -> Option<Self> {
        let mut it = s.split_whitespace().map(|w| w.parse::<u64>().ok());
        let scan = CompactScan {
            offset: it.next()??,
            sum: it.next()??,
            last: it.next()??,
        };
        Some(scan)
    }
}

/// The tokens this session spent before its compactions: the context each
/// `/compact` (manual or automatic) dropped, summed over every one — `0` for a
/// session that never compacted, or a transcript that cannot be read.
pub fn compacted_tokens(path: &Path) -> u64 {
    compacted_tokens_cached(path, None)
}

/// [`compacted_tokens`], picking up where the last call left off when
/// `cache` names a directory to keep a scan in: the whole transcript is read
/// once, and after that only what was appended since (a planets transcript
/// is 16 MB; the status line runs every turn). A transcript that shrank is
/// read again from the top.
pub fn compacted_tokens_cached(path: &Path, cache: Option<&Path>) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return 0;
    };
    let slot = cache.map(|dir| dir.join(cache_name(path)));
    let mut scan = slot
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| CompactScan::parse(&s))
        .filter(|s| s.offset <= len)
        .unwrap_or_default();
    let before = scan;
    if scan.offset < len && file.seek(SeekFrom::Start(scan.offset)).is_ok() {
        let mut buf = Vec::with_capacity((len - scan.offset) as usize);
        if (&mut file)
            .take(len - scan.offset)
            .read_to_end(&mut buf)
            .is_ok()
        {
            scan.feed(&buf);
        }
    }
    if let Some(slot) = slot
        && scan != before
    {
        let dir = slot.parent().unwrap_or(Path::new("."));
        let _ = std::fs::create_dir_all(dir);
        // A new transcript: a moment to drop the scans nobody has read in a
        // month, so the directory does not grow by one file per session
        // forever.
        if !slot.exists() {
            prune_scans(dir, SCAN_KEPT_FOR);
        }
        let tmp = slot.with_extension("tmp");
        let body = format!("{} {} {}\n", scan.offset, scan.sum, scan.last);
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &slot);
        }
    }
    scan.sum
}

/// One cache file per transcript path: its file name (the session id) and a
/// hash of the whole path, so two config dirs holding the same id never share.
/// FNV-1a rather than `DefaultHasher`, whose output may change with any Rust
/// release and would orphan every scan.
fn cache_name(path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}-{h:016x}.scan")
}

/// How long a scan nobody reads is kept.
const SCAN_KEPT_FOR: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Remove the scans in `dir` last written longer ago than `kept_for`. A
/// session that is resumed after that is scanned again from the top.
fn prune_scans(dir: &Path, kept_for: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > kept_for);
        if old && path.extension().is_some_and(|e| e == "scan") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Where [`compacted_tokens_cached`] keeps its scans:
/// `$XDG_CACHE_HOME/giverny/compactions` (`~/.cache/…`).
pub fn compact_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("giverny").join("compactions"))
}

/// The session's own count, and whether it is only an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    pub tokens: u64,
    pub estimated: bool,
}

impl Session {
    pub fn measured(tokens: u64) -> Self {
        Session {
            tokens,
            estimated: false,
        }
    }
}

/// The session's own count: `context_window.total_input_tokens` off the status
/// line's stdin when it is there (the same number, a turn fresher), else its
/// transcript — except right after a compaction, when neither knows yet
/// ([`after_compaction`]).
pub fn session_tokens(payload: &Value, transcript: Option<&Path>) -> Option<Session> {
    if let Some(n) = transcript.and_then(after_compaction) {
        return Some(Session {
            tokens: n,
            estimated: true,
        });
    }
    let live = payload
        .get("context_window")
        .and_then(|c| c.get("total_input_tokens"))
        .and_then(Value::as_f64);
    if let Some(v) = live
        && v > 0.0
    {
        return Some(Session::measured(v as u64));
    }
    transcript.and_then(tokens_of).map(Session::measured)
}

/// What the session carries now that it has compacted and sent nothing since,
/// or `None` when a reply came after its last compaction (or it never
/// compacted).
///
/// Until the next request nobody has measured the new context:
/// Claude Code's `total_input_tokens` is the last usage among the messages it
/// kept — stale or `0` — and the transcript's last usage is the one before the
/// boundary. So this estimates it: the boundary's `postTokens`, Claude Code's
/// count of the kept messages alone, plus the session's first reply's context
/// for the system prompt, tools and memory that count leaves out. Measured on
/// 30 compactions in 11 transcripts, 2026-10-12: that lands within ~10% of the
/// next reply's real context (58.3k against 59k in ToLaD `6c6ccfc0`). A
/// boundary without `postTokens` is left to the old count.
pub fn after_compaction(path: &Path) -> Option<u64> {
    let post = tail_find(path, |buf| {
        buf.split(|&c| c == b'\n').rev().find_map(|line| {
            if let Some(post) = boundary_field(line, "postTokens") {
                Some(post)
            } else {
                line_tokens(line).map(|_| None)
            }
        })
    })??;
    Some(post + first_tokens(path).unwrap_or(0))
}

/// The context of a transcript's first reply, read from its head.
fn first_tokens(path: &Path) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .find_map(|line| line_tokens(&line))
}

/// Every `subagents/` dir of this session: beside its transcript, and under
/// any project dir of the config dir that holds the session id.
pub fn session_subagent_dirs(
    transcript: Option<&Path>,
    config_dir: Option<&Path>,
    session_id: Option<&str>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(t) = transcript {
        push(t.with_extension("").join("subagents"));
    }
    if let (Some(cfg), Some(sid)) = (config_dir, session_id.filter(|s| !s.is_empty()))
        && let Ok(entries) = std::fs::read_dir(cfg.join("projects"))
    {
        for e in entries.flatten() {
            push(e.path().join(sid).join("subagents"));
        }
    }
    out
}

/// Every subagent transcript in these dirs, each file counted once however
/// many dirs link to it.
pub fn subagent_transcripts(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        for id in crate::subagents::list_agent_ids(dir) {
            let p = crate::subagents::agent_transcript(dir, &id);
            let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if seen.insert(key) {
                out.push(p);
            }
        }
    }
    out
}

/// `(session, subagents, total)`: the session's count, the sum of every
/// subagent's, and everything added — the session, what it spent before its
/// compactions (`compacted`) and its subagents — so the parts
/// always add up (giverny#95). `None` for all three when nothing could be
/// counted; `subagents` is `Some(0)` when the session has none.
pub fn session_subagents_total(
    session: Option<u64>,
    compacted: u64,
    subagents: &[PathBuf],
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let workers: Vec<u64> = subagents.iter().filter_map(|p| tokens_of(p)).collect();
    if session.is_none() && compacted == 0 && workers.is_empty() {
        return (None, None, None);
    }
    let sub: u64 = workers.iter().sum();
    (
        session,
        Some(sub),
        Some(session.unwrap_or(0) + compacted + sub),
    )
}

/// Claude Code's compact count — `842`, `13.5k`, `124.8k`, `1.2M` — with a
/// capital `M` so a total never reads as minutes.
pub fn fmt_tokens(n: u64) -> String {
    for (floor, div, suffix) in [
        (999_950_000u64, 1_000_000_000f64, "B"),
        (999_950, 1_000_000.0, "M"),
        (1000, 1000.0, "k"),
    ] {
        if n >= floor {
            let v = format!("{:.1}", n as f64 / div);
            let v = v.strip_suffix(".0").unwrap_or(&v);
            return format!("{v}{suffix}");
        }
    }
    n.to_string()
}

/// The status-line segments: `session: <n>` always once anything was counted,
/// with `(+<n>)` after it once the session has compacted — what it spent
/// before — then `subagents: <n>` only when the subagents have
/// tokens and `total: <n>` only when there is something besides the session's
/// own count to add (giverny#95): otherwise the total would only repeat it.
pub fn segments(
    session: Option<Session>,
    compacted: u64,
    subagents: Option<u64>,
    total: Option<u64>,
) -> Vec<String> {
    let sub = subagents.unwrap_or(0);
    if session.is_none() && compacted == 0 && sub == 0 {
        return Vec::new();
    }
    let mut head = match session {
        Some(Session {
            tokens,
            estimated: true,
        }) => format!("session: ~{}", fmt_tokens(tokens)),
        _ => format!("session: {}", fmt_tokens(session.map_or(0, |s| s.tokens))),
    };
    if compacted > 0 {
        head += &format!(" (+{})", fmt_tokens(compacted));
    }
    let mut out = vec![head];
    if sub > 0 {
        out.push(format!("subagents: {}", fmt_tokens(sub)));
    }
    if sub > 0 || compacted > 0 {
        out.push(format!("total: {}", fmt_tokens(total.unwrap_or(0))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-tokens-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn asst(usage: Value) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": "hi"}], "usage": usage}})
        .to_string()
    }

    #[test]
    fn the_last_response_is_assigned_not_summed_and_output_is_left_out() {
        let d = tmpdir("last");
        let p = d.join("s.jsonl");
        let lines = [
            json!({"type": "user", "message": {"content": "go"}}).to_string(),
            asst(
                json!({"input_tokens": 10, "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 1000, "output_tokens": 5000}),
            ),
            asst(
                json!({"input_tokens": 2, "cache_creation_input_tokens": 300,
                        "cache_read_input_tokens": 2000, "output_tokens": 9}),
            ),
            // A trailing tool result, and a zero usage that says nothing.
            json!({"type": "user", "message": {"content": [{"type": "tool_result"}]}}).to_string(),
            asst(json!({"input_tokens": 0, "output_tokens": 0})),
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert_eq!(tokens_of(&p), Some(2302));
    }

    #[test]
    fn synthetic_and_non_assistant_usages_are_skipped() {
        let d = tmpdir("synth");
        let p = d.join("s.jsonl");
        let synth =
            json!({"type": "assistant", "message": {"role": "assistant", "model": "<synthetic>",
            "usage": {"input_tokens": 99999}}})
            .to_string();
        // `"usage":` before any `"assistant"` on the line: not an assistant usage.
        let other = r#"{"usage":{"input_tokens":7777},"type":"assistant"}"#.to_string();
        let lines = [asst(json!({"input_tokens": 40})), synth, other];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(tokens_of(&p), Some(40));
    }

    #[test]
    fn the_last_real_iteration_wins_over_the_top_level() {
        let u = json!({"input_tokens": 1, "cache_read_input_tokens": 1000,
            "iterations": [
                {"type": "message", "input_tokens": 5, "cache_creation_input_tokens": 0,
                 "cache_read_input_tokens": 50, "output_tokens": 3},
                {"type": "compaction", "input_tokens": 9000}]});
        assert_eq!(usage_numbers(&u), (5, 0, 50));
        // An unexpected last entry: the top level wins.
        let u = json!({"input_tokens": 1, "iterations": [{"type": "weird", "input_tokens": 5}]});
        assert_eq!(usage_numbers(&u), (1, 0, 0));
        // A missing field on the iteration: the top level wins.
        let u = json!({"input_tokens": 1, "iterations": [{"type": "message", "input_tokens": 5}]});
        assert_eq!(usage_numbers(&u), (1, 0, 0));
    }

    #[test]
    fn a_usage_far_from_the_tail_is_still_found() {
        let d = tmpdir("far");
        let p = d.join("s.jsonl");
        let big = "x".repeat(300_000);
        let mut body = asst(json!({"input_tokens": 1234})) + "\n";
        for _ in 0..3 {
            body += &json!({"type": "user", "message": {"content": big}}).to_string();
            body += "\n";
        }
        std::fs::write(&p, body).unwrap();
        assert_eq!(tokens_of(&p), Some(1234));
        assert_eq!(tokens_of(&d.join("missing.jsonl")), None);
    }

    #[test]
    fn stdin_context_is_the_session_count_when_present() {
        let d = tmpdir("stdin");
        let p = d.join("s.jsonl");
        std::fs::write(&p, asst(json!({"input_tokens": 70}))).unwrap();
        let live = json!({"context_window": {"total_input_tokens": 500}});
        assert_eq!(
            session_tokens(&live, Some(&p)),
            Some(Session::measured(500))
        );
        assert_eq!(
            session_tokens(&json!({}), Some(&p)),
            Some(Session::measured(70))
        );
        let zero = json!({"context_window": {"total_input_tokens": 0}});
        assert_eq!(session_tokens(&zero, Some(&p)), Some(Session::measured(70)));
        assert_eq!(session_tokens(&json!({}), None), None);
    }

    #[test]
    fn total_adds_every_subagent_once() {
        let cfg = tmpdir("total");
        let proj = cfg.join("projects/-home-x");
        let sub = proj.join("sid1/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let t = proj.join("sid1.jsonl");
        std::fs::write(&t, asst(json!({"input_tokens": 1000}))).unwrap();
        std::fs::write(
            sub.join("agent-a.jsonl"),
            asst(json!({"input_tokens": 200})),
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-b.jsonl"),
            asst(json!({"cache_read_input_tokens": 30})),
        )
        .unwrap();
        std::fs::write(sub.join("agent-b.meta.json"), "{}").unwrap();
        // The same worker linked into a second project dir is counted once.
        #[cfg(unix)]
        {
            let other = cfg.join("projects/-home-y/sid1/subagents");
            std::fs::create_dir_all(&other).unwrap();
            std::os::unix::fs::symlink(sub.join("agent-a.jsonl"), other.join("agent-a.jsonl"))
                .unwrap();
        }
        let dirs = session_subagent_dirs(Some(&t), Some(&cfg), Some("sid1"));
        let files = subagent_transcripts(&dirs);
        assert_eq!(files.len(), 2);
        let (s, sub, total) = session_subagents_total(
            session_tokens(&json!({}), Some(&t)).map(|s| s.tokens),
            0,
            &files,
        );
        assert_eq!((s, sub, total), (Some(1000), Some(230), Some(1230)));
        assert_eq!(
            segments(s.map(Session::measured), 0, sub, total),
            vec![
                "session: 1k".to_string(),
                "subagents: 230".to_string(),
                "total: 1.2k".to_string()
            ]
        );
        // A session with no subagent tokens shows only its own count.
        let (s, sub, total) = session_subagents_total(Some(1000), 0, &[]);
        assert_eq!((s, sub, total), (Some(1000), Some(0), Some(1000)));
        assert_eq!(
            segments(s.map(Session::measured), 0, sub, total),
            vec!["session: 1k".to_string()]
        );
        // No session and no subagents: nothing to say.
        assert_eq!(session_subagents_total(None, 0, &[]), (None, None, None));
        assert!(segments(None, 0, None, None).is_empty());
    }

    fn boundary(pre: Option<u64>) -> String {
        let mut meta = json!({"trigger": "manual", "postTokens": 7604});
        if let Some(n) = pre {
            meta["preTokens"] = json!(n);
        }
        json!({"parentUuid": null, "type": "system", "subtype": "compact_boundary",
            "content": "Conversation compacted", "compactMetadata": meta, "sessionId": "s"})
        .to_string()
    }

    #[test]
    fn a_session_that_never_compacted_looks_as_before() {
        let d = tmpdir("nocompact");
        let p = d.join("s.jsonl");
        // A user line merely mentioning the subtype is not a boundary.
        let lines = [
            asst(json!({"input_tokens": 95_500})),
            json!({"type": "user", "message": {"content": "grep compact_boundary"}}).to_string(),
            json!({"type": "user", "message": {"content": "\"compact_boundary\""}}).to_string(),
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert_eq!(compacted_tokens(&p), 0);
        assert_eq!(compacted_tokens(&d.join("missing.jsonl")), 0);
        let (s, sub, total) = session_subagents_total(Some(95_500), 0, &[]);
        assert_eq!(
            segments(s.map(Session::measured), 0, sub, total),
            vec!["session: 95.5k".to_string()]
        );
    }

    #[test]
    fn every_compaction_is_summed_and_added_to_the_total() {
        let d = tmpdir("compact");
        let p = d.join("s.jsonl");
        let lines = [
            asst(json!({"input_tokens": 334_163})),
            boundary(Some(335_210)),
            asst(json!({"input_tokens": 52_916})),
            asst(json!({"input_tokens": 120_000})),
            // No preTokens: the last context before it stands in.
            boundary(None),
            asst(json!({"input_tokens": 95_500})),
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let compacted = compacted_tokens(&p);
        assert_eq!(compacted, 335_210 + 120_000);
        // The session's own count is still the latest context, untouched.
        assert_eq!(tokens_of(&p), Some(95_500));
        let (s, sub, total) = session_subagents_total(tokens_of(&p), compacted, &[]);
        assert_eq!((s, sub, total), (Some(95_500), Some(0), Some(550_710)));
        assert_eq!(
            segments(s.map(Session::measured), compacted, sub, total),
            vec![
                "session: 95.5k (+455.2k)".to_string(),
                "total: 550.7k".to_string()
            ]
        );
        // With subagents, all three parts are in the total.
        assert_eq!(
            segments(
                Some(Session::measured(95_500)),
                compacted,
                Some(1000),
                Some(551_710)
            ),
            vec![
                "session: 95.5k (+455.2k)".to_string(),
                "subagents: 1k".to_string(),
                "total: 551.7k".to_string()
            ]
        );
    }

    #[test]
    fn right_after_a_compaction_the_session_is_estimated_until_the_next_reply() {
        let d = tmpdir("aftercompact");
        let p = d.join("s.jsonl");
        let user = |text: &str| json!({"type": "user", "message": {"content": text}}).to_string();
        let mut lines = vec![
            user("go"),
            // The first reply: system prompt, tools and memory.
            asst(json!({"input_tokens": 3, "cache_creation_input_tokens": 41_000})),
            asst(json!({"input_tokens": 334_163})),
            boundary(Some(335_210)),
            user("This session is being continued from a previous conversation"),
            user("<command-name>/compact</command-name>"),
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        // Claude Code still reports the kept messages' old usage, or nothing.
        let stale = json!({"context_window": {"total_input_tokens": 334_163}});
        let want = Some(Session {
            tokens: 7604 + 41_003,
            estimated: true,
        });
        assert_eq!(after_compaction(&p), Some(48_607));
        assert_eq!(session_tokens(&stale, Some(&p)), want);
        assert_eq!(session_tokens(&json!({}), Some(&p)), want);
        let compacted = compacted_tokens(&p);
        let (_, sub, total) = session_subagents_total(Some(48_607), compacted, &[]);
        assert_eq!(
            segments(want, compacted, sub, total),
            vec![
                "session: ~48.6k (+335.2k)".to_string(),
                "total: 383.8k".to_string()
            ]
        );
        // The next reply measures it, and the estimate is gone.
        lines.push(asst(json!({"input_tokens": 52_916})));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert_eq!(after_compaction(&p), None);
        let live = json!({"context_window": {"total_input_tokens": 52_916}});
        assert_eq!(
            session_tokens(&live, Some(&p)),
            Some(Session::measured(52_916))
        );
        // A boundary without `postTokens` is left to the old count.
        let bare = json!({"type": "system", "subtype": "compact_boundary",
            "compactMetadata": {"trigger": "manual", "preTokens": 9}})
        .to_string();
        std::fs::write(
            &p,
            [asst(json!({"input_tokens": 70})), bare].join("\n") + "\n",
        )
        .unwrap();
        assert_eq!(after_compaction(&p), None);
        assert_eq!(
            session_tokens(&json!({}), Some(&p)),
            Some(Session::measured(70))
        );
    }

    #[test]
    fn the_cached_scan_reads_only_what_was_appended() {
        let d = tmpdir("cache");
        let cache = d.join("cache");
        let p = d.join("s.jsonl");
        let mut body = asst(json!({"input_tokens": 10})) + "\n" + &boundary(Some(300_000)) + "\n";
        std::fs::write(&p, &body).unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 300_000);
        let slot = std::fs::read_dir(&cache)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let saved = std::fs::read_to_string(&slot).unwrap();
        assert!(
            saved.starts_with(&format!("{} 300000 ", body.len())),
            "{saved}"
        );
        // A half-written line is left for the next read.
        let second = boundary(Some(5000));
        std::fs::write(&p, body.clone() + &second[..20]).unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 300_000);
        body += &(second + "\n");
        std::fs::write(&p, &body).unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 305_000);
        // The cache really is read: a doctored one is believed while it fits.
        std::fs::write(&slot, format!("{} 7 0\n", body.len())).unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 7);
        // A transcript shorter than the scan is rescanned from the top.
        std::fs::write(&p, asst(json!({"input_tokens": 1})) + "\n").unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 0);
    }

    /// The cache's names survive a toolchain bump, and a scan nobody has
    /// read in a month is dropped when a new transcript is first scanned.
    #[test]
    fn scans_keep_their_names_and_old_ones_go() {
        assert_eq!(
            cache_name(Path::new("/home/x/.claude/projects/p/abc.jsonl")),
            format!("abc-{:016x}.scan", {
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for b in "/home/x/.claude/projects/p/abc.jsonl".bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
                }
                h
            })
        );

        let d = tmpdir("prune");
        let cache = d.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let (old, recent, other) = (
            cache.join("old-1.scan"),
            cache.join("recent-2.scan"),
            cache.join("notes.txt"),
        );
        for f in [&old, &recent, &other] {
            std::fs::write(f, "1 0 0\n").unwrap();
        }
        let long_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
        for f in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        let p = d.join("new.jsonl");
        std::fs::write(&p, boundary(Some(9)) + "\n").unwrap();
        assert_eq!(compacted_tokens_cached(&p, Some(&cache)), 9);
        assert!(!old.exists(), "a month-old scan goes");
        assert!(recent.exists(), "a recent one stays");
        assert!(other.exists(), "and nothing but scans is touched");
    }

    #[test]
    fn the_last_reply_says_when_it_was_and_what_it_cached() {
        let d = tmpdir("last-reply");
        let t = d.join("s.jsonl");
        let reply = |at: &str, sidechain: bool, usage: Value| {
            json!({"type": "assistant", "timestamp": at, "isSidechain": sidechain,
                "message": {"role": "assistant", "model": "claude-opus-5-5",
                    "content": [{"type": "text", "text": "hi"}], "usage": usage}})
            .to_string()
        };
        let cached = |k: &str| {
            json!({"input_tokens": 2, "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 90_000, "output_tokens": 500,
                "cache_creation": {k: 1000}})
        };
        let lines = [
            reply(
                "2026-10-06T10:00:00Z",
                false,
                cached("ephemeral_5m_input_tokens"),
            ),
            reply(
                "2026-10-06T11:00:00Z",
                false,
                cached("ephemeral_1h_input_tokens"),
            ),
            // A sidechain reply after it is not the conversation's.
            reply(
                "2026-10-06T12:00:00Z",
                true,
                cached("ephemeral_5m_input_tokens"),
            ),
            json!({"type": "user", "timestamp": "2026-10-06T12:30:00Z"}).to_string(),
        ];
        std::fs::write(&t, lines.join("\n") + "\n").unwrap();
        let at = "2026-10-06T11:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        assert_eq!(
            last_reply(&t),
            Some(LastReply {
                at_ms: at,
                ttl_ms: Some(3_600_000),
                recache: Some(91_502)
            })
        );
        // A reply that wrote nothing to the cache leaves the TTL unknown.
        std::fs::write(
            &t,
            reply(
                "2026-10-06T10:00:00Z",
                false,
                json!({"cache_read_input_tokens": 5}),
            ),
        )
        .unwrap();
        assert_eq!(last_reply(&t).map(|r| r.ttl_ms), Some(None));
        assert_eq!(last_reply(&d.join("missing.jsonl")), None);
    }

    #[test]
    fn a_compaction_after_the_last_reply_stands_in_for_it() {
        let d = tmpdir("last-reply-compact");
        let t = d.join("s.jsonl");
        let reply = |at: &str, usage: Value| {
            json!({"type": "assistant", "timestamp": at,
                "message": {"role": "assistant", "model": "claude-opus-5-5",
                    "content": [{"type": "text", "text": "hi"}], "usage": usage}})
            .to_string()
        };
        let compaction = |post: Option<u64>| {
            let mut b: Value = serde_json::from_str(&boundary(Some(306_000))).unwrap();
            b["timestamp"] = json!("2026-10-06T11:30:00Z");
            match post {
                Some(n) => b["compactMetadata"]["postTokens"] = json!(n),
                None => {
                    b["compactMetadata"]
                        .as_object_mut()
                        .unwrap()
                        .remove("postTokens");
                }
            }
            let lines = [
                // The session's first reply: system prompt, tools, memory.
                reply(
                    "2026-10-06T09:00:00Z",
                    json!({"cache_creation_input_tokens": 41_000}),
                ),
                reply(
                    "2026-10-06T11:00:00Z",
                    json!({"input_tokens": 2, "cache_read_input_tokens": 305_000,
                        "output_tokens": 500, "cache_creation_input_tokens": 1000,
                        "cache_creation": {"ephemeral_1h_input_tokens": 1000}}),
                ),
                b.to_string(),
                json!({"type": "user", "isCompactSummary": true}).to_string(),
            ];
            std::fs::write(&t, lines.join("\n") + "\n").unwrap();
            last_reply(&t)
        };
        let at = "2026-10-06T11:30:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        // Cold an hour after the compaction, and the next message re-caches
        // the compacted context, not the 306k before it.
        assert_eq!(
            compaction(Some(7604)),
            Some(LastReply {
                at_ms: at,
                ttl_ms: Some(3_600_000),
                recache: Some(7604 + 41_000)
            })
        );
        // A boundary that does not say what it kept leaves the figure unknown.
        assert_eq!(compaction(None).map(|r| r.recache), Some(None));
    }

    #[test]
    fn counts_format_like_claude_code() {
        assert_eq!(fmt_tokens(842), "842");
        assert_eq!(fmt_tokens(1000), "1k");
        assert_eq!(fmt_tokens(13_450), "13.4k");
        assert_eq!(fmt_tokens(124_832), "124.8k");
        assert_eq!(fmt_tokens(999_949), "999.9k");
        assert_eq!(fmt_tokens(999_950), "1M");
        assert_eq!(fmt_tokens(1_234_567), "1.2M");
        assert_eq!(fmt_tokens(2_500_000_000), "2.5B");
    }
}
