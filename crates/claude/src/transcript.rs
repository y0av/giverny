//! A worker's transcript, rendered for a person the way Claude Code draws a
//! worker's own view.
//!
//! Claude Code cannot open a subagent by id, and keeps no view of one whose
//! turn has ended, so Giverny reads the worker's own `agent-<id>.jsonl`
//! instead and draws it as Claude Code's subagent view does (as captured from
//! Claude Code 2.1.304):
//!
//! ```text
//! ❯ the prompt the worker was given, on a grey band
//!
//! ● Read(/tmp/proj/notes.txt)
//!   ⎿  Read 4 lines
//!
//! ● Bash(seq 1 40)
//!   ⎿  1
//!      2
//!      3
//!      … +37 lines
//!
//! ● What the worker said, `code` in its own colour.
//! ```
//!
//! A tool call's `●` is green once its result is back, red when the result
//! is an error, grey while it runs ([`settle`]). Thinking, attachments and
//! bookkeeping lines are left out, and so is the reply to a worker's
//! `SubagentHandback`: its report is shown as what the worker said. A line
//! sent to the worker after its prompt is drawn as a prompt, in whichever
//! shape it landed ([`Render::line`]). One still on its way is the page's to
//! draw, in the same shape and dimmed ([`echo_rows`]).
//!
//! The worker page a row of the management panel opens draws it: a prompt
//! whole, a tool's output folded to its first lines.
//!
//! [`Render`] is the pure half — one JSONL line in, zero or more rows out —
//! so it is tested without a file; [`Row::to_ansi`] paints a row for the
//! page's grid. [`Tail`] is the `tail -f` under a live page: it reads a
//! whole line at a time, holds back a half-written last line until its
//! newline arrives, and notices when the file is truncated or replaced.
//!
//! As everywhere in this crate, the lines are Claude Code's private format:
//! fields are pulled out one at a time, and a line that does not parse is
//! skipped rather than stopping the view.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// What Claude Code wraps a line in when it lands in a worker's turn as a
/// `user` record: its parent's (`SendMessage`), then one typed in the
/// worker's own view (Claude Code 2.1.317).
const SENT_WRAPPERS: [(&str, &str); 2] = [
    (
        "The coordinator sent a message while you were working:\n",
        "\n\nAddress this before completing your current task.",
    ),
    (
        "The user sent a new message while you were working:\n",
        "\n\nThis is how Claude Code surfaces messages the user sends mid-turn \u{2014} within \
         the running turn, often alongside the next tool result, rather than as a separate \
         conversation turn. Address the message above as you continue this turn.",
    ),
];

/// Lines of a tool result shown before it is folded: Claude Code's three.
const RESULT_LINES: usize = 3;
/// Widest single line we print from a result, in characters.
const LINE_MAX: usize = 200;
/// Widest argument a tool call's line shows, in characters.
const CALL_MAX: usize = 300;

/// The bullet a tool call starts with, as Claude Code draws it.
pub const BULLET: &str = "● ";
/// What a prompt starts with.
pub const PROMPT: &str = "❯ ";
/// What a tool's result hangs off.
pub const RESULT: &str = "  ⎿  ";
/// The indent of a result's later lines.
const RESULT_MORE: &str = "     ";
/// The indent of a reply's or a prompt's later lines.
const MORE: &str = "  ";

/// Options for one render.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    /// ANSI colour. Off for tests.
    pub color: bool,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !code.is_empty() {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

/// Where a tool call stands, which is its bullet's colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// No result yet: it runs.
    Running,
    Done,
    Failed,
}

/// What a span is, which is how it is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The worker's own words, a tool's argument and its output.
    Plain,
    /// A tool's name, a `**bold**` or a heading in a reply.
    Strong,
    /// `inline code` and code blocks in a reply.
    Code,
    /// Fold marks, `(No output)`, a hint.
    Dim,
    /// Tool output that is an error.
    Error,
    /// A prompt's text: drawn on the prompt band.
    Prompt,
    /// A line sent and not yet in the transcript: a prompt, dimmed.
    Echo,
    /// The `●` before what the worker said.
    Said,
    /// The `●` before a tool call.
    Bullet(Call),
}

/// A run of text in one tone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub tone: Tone,
    pub text: String,
}

/// One rendered row: spans, drawn on one line (wrapped if it must).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub spans: Vec<Span>,
    /// The row starts an entry (a prompt, a call, a reply): a blank line
    /// goes before it, unless it is the first.
    pub starts: bool,
    /// On a tool call's row: the call's id, so its result can settle the
    /// bullet ([`settle`]).
    pub call: Option<String>,
    /// On a result's first row: the call it answers, and whether it failed.
    pub answers: Option<(String, bool)>,
}

impl Row {
    fn of(spans: Vec<Span>) -> Row {
        Row {
            spans,
            ..Row::default()
        }
    }

    /// A row in one tone.
    pub fn one(tone: Tone, text: impl Into<String>) -> Row {
        Row::of(vec![span(tone, text)])
    }

    /// The row's text, without colour.
    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// The row's tone when it is part of a prompt, drawn on its band.
    pub fn band(&self) -> Option<Tone> {
        let tone = self.spans.first()?.tone;
        matches!(tone, Tone::Prompt | Tone::Echo).then_some(tone)
    }

    /// The row as a terminal line.
    pub fn to_ansi(&self, style: Style) -> String {
        self.spans
            .iter()
            .map(|s| style.paint(ansi(s.tone), &s.text))
            .collect()
    }
}

/// The ANSI colour of a tone; Claude Code's own for its marks.
fn ansi(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "",
        Tone::Strong | Tone::Said => BOLD,
        Tone::Code => "\x1b[38;2;177;185;249m",
        Tone::Dim | Tone::Bullet(Call::Running) => DIM,
        Tone::Error | Tone::Bullet(Call::Failed) => "\x1b[38;2;255;107;128m",
        Tone::Prompt => "\x1b[48;2;55;55;55m",
        Tone::Echo => "\x1b[2m\x1b[48;2;55;55;55m",
        Tone::Bullet(Call::Done) => "\x1b[38;2;78;186;101m",
    }
}

fn span(tone: Tone, text: impl Into<String>) -> Span {
    Span {
        tone,
        text: text.into(),
    }
}

/// Settle the bullets of the tool calls in `rows` whose results are there:
/// green, or red for a failed one. Rows are rendered a line at a time and a
/// call's result comes on a later line, so this runs over the rows so far.
pub fn settle(rows: &mut [Row]) {
    let mut failed = std::collections::HashMap::new();
    for r in rows.iter() {
        if let Some((id, err)) = &r.answers {
            failed.insert(id.clone(), *err);
        }
    }
    for r in rows.iter_mut() {
        let Some(id) = &r.call else { continue };
        let Some(err) = failed.get(id) else { continue };
        if let Some(s) = r.spans.first_mut() {
            s.tone = Tone::Bullet(if *err { Call::Failed } else { Call::Done });
        }
    }
}

/// The columns a row's wrapped lines hang under: its marks (`● `, `❯ `,
/// `  ⎿  `) and the spaces around them, as Claude Code indents a long line.
fn hang(row: &Row) -> usize {
    row.plain()
        .chars()
        .take_while(|c| c.is_whitespace() || matches!(c, '●' | '❯' | '⎿'))
        .count()
}

/// `row` wrapped at `cols` columns, its later lines hung under its text
/// rather than under its mark, at a space where there is one. A row that
/// fits comes back as it is.
pub fn wrap(row: &Row, cols: usize) -> Vec<Row> {
    let chars: Vec<(Tone, char)> = row
        .spans
        .iter()
        .flat_map(|s| s.text.chars().map(move |c| (s.tone, c)))
        .collect();
    if cols == 0 || chars.len() <= cols {
        return vec![row.clone()];
    }
    let hang = hang(row).min(cols / 2);
    let pad = row.spans.first().map_or(Tone::Plain, |s| s.tone);
    let mut out: Vec<Row> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let room = if out.is_empty() { cols } else { cols - hang };
        let mut end = (at + room).min(chars.len());
        // Break at the last space that fits, never inside the marks.
        let marks = if out.is_empty() { hang } else { 0 };
        if end < chars.len()
            && chars[end].1 != ' '
            && let Some(sp) = chars[at..end].iter().rposition(|(_, c)| *c == ' ')
            && sp > marks
        {
            end = at + sp;
        }
        let mut spans: Vec<Span> = Vec::new();
        if !out.is_empty() && hang > 0 {
            spans.push(span(pad, " ".repeat(hang)));
        }
        for &(tone, c) in &chars[at..end] {
            match spans.last_mut() {
                Some(s) if s.tone == tone => s.text.push(c),
                _ => spans.push(span(tone, c.to_string())),
            }
        }
        let mut r = if out.is_empty() {
            Row {
                spans,
                ..row.clone()
            }
        } else {
            Row::of(spans)
        };
        if !out.is_empty() {
            r.starts = false;
        }
        out.push(r);
        at = end;
        // The space a line was broken at is not carried to the next one.
        if at < chars.len() && chars[at].1 == ' ' {
            at += 1;
        }
    }
    out
}

/// Add `new` to `rows` as Claude Code lays a transcript out: a tool's
/// result goes under its own call, even when the worker made several calls
/// at once and their results came after all of them; anything else goes at
/// the end. Bullets are settled ([`settle`]) as results come in.
pub fn push(rows: &mut Vec<Row>, new: Vec<Row>) {
    let mut groups: Vec<Vec<Row>> = Vec::new();
    for r in new {
        match groups.last_mut() {
            Some(g) if !r.starts && r.answers.is_none() => g.push(r),
            _ => groups.push(vec![r]),
        }
    }
    let mut settled = false;
    for g in groups {
        let call = g[0].answers.as_ref().map(|(id, _)| id.clone());
        let at = call.and_then(|id| {
            let i = rows
                .iter()
                .rposition(|r| r.call.as_deref() == Some(id.as_str()))?;
            Some(
                rows[i + 1..]
                    .iter()
                    .position(|r| r.starts)
                    .map_or(rows.len(), |n| i + 1 + n),
            )
        });
        settled |= g[0].answers.is_some();
        match at {
            Some(at) => {
                rows.splice(at..at, g);
            }
            None => rows.extend(g),
        }
    }
    if settled {
        settle(rows);
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `… +N lines`, as Claude Code marks a fold.
fn more(n: usize) -> String {
    format!("… +{n} line{}", if n == 1 { "" } else { "s" })
}

/// A tool call as Claude Code titles it: the name, and the one argument
/// that says what it does — `Bash(cargo test)`, `Read(/a/b.rs)`.
pub fn call_title(tool_use: &Value) -> (String, String) {
    let name = str_of(tool_use, "name").unwrap_or("Tool");
    let input = tool_use.get("input").unwrap_or(&Value::Null);
    let name = match name.strip_prefix("mcp__") {
        Some(rest) => match rest.split_once("__") {
            Some((server, tool)) => format!("{server} - {tool} (MCP)"),
            None => rest.to_string(),
        },
        None if name == "TodoWrite" => return ("Update Todos".into(), String::new()),
        None => name.to_string(),
    };
    let arg = match name.as_str() {
        "Grep" => str_of(input, "pattern").map(|p| match str_of(input, "path") {
            Some(path) => format!("pattern: \"{p}\", path: \"{path}\""),
            None => format!("pattern: \"{p}\""),
        }),
        "Agent" | "Task" => str_of(input, "description").map(String::from),
        _ => [
            "command",
            "file_path",
            "notebook_path",
            "path",
            "pattern",
            "url",
            "query",
            "skill",
            "to",
            "description",
            "prompt",
        ]
        .iter()
        .find_map(|k| str_of(input, k))
        .map(String::from),
    };
    let arg = arg
        .map(|a| {
            clip(
                &a.split_whitespace().collect::<Vec<_>>().join(" "),
                CALL_MAX,
            )
        })
        .unwrap_or_default();
    (name, arg)
}

/// A tool result's `content`: a string, or a list of `{type:"text"}` blocks
/// (anything else — an image — is named rather than shown).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What Claude Code says in place of a file tool's output: `Read 4 lines`,
/// `Wrote 12 lines to …`, `Updated … with 2 additions and 1 removal`.
fn file_summary(r: &Value) -> Option<String> {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if let Some(n) = r.pointer("/file/numLines").and_then(Value::as_u64) {
        return Some(format!("Read {}", plural(n as usize, "line", "lines")));
    }
    let path = str_of(r, "filePath")?;
    if r.get("type").and_then(Value::as_str) == Some("create") {
        let n = str_of(r, "content").map_or(0, |c| c.lines().count());
        return Some(format!("Wrote {} to {path}", plural(n, "line", "lines")));
    }
    let hunks = r.get("structuredPatch")?.as_array()?;
    let (mut add, mut del) = (0, 0);
    for line in hunks
        .iter()
        .filter_map(|h| h.get("lines")?.as_array())
        .flatten()
        .filter_map(Value::as_str)
    {
        if line.starts_with('+') {
            add += 1;
        } else if line.starts_with('-') {
            del += 1;
        }
    }
    Some(format!(
        "Updated {path} with {} and {}",
        plural(add, "addition", "additions"),
        plural(del, "removal", "removals")
    ))
}

/// `text` as a prompt: `❯ ` on its first line, the rest under it, all on
/// the band; folded to `keep` lines when given.
pub fn prompt_rows(text: &str, keep: Option<usize>) -> Vec<Row> {
    let lines: Vec<&str> = text.trim().lines().map(str::trim_end).collect();
    let keep_n = keep.unwrap_or(usize::MAX);
    let mut out: Vec<Row> = lines
        .iter()
        .take(keep_n)
        .enumerate()
        .map(|(i, l)| {
            let lead = if i == 0 { PROMPT } else { MORE };
            Row::one(Tone::Prompt, format!("{lead}{l}"))
        })
        .collect();
    if out.is_empty() {
        out.push(Row::one(Tone::Prompt, PROMPT.trim_end()));
    }
    if lines.len() > keep_n {
        out.push(Row::one(
            Tone::Dim,
            format!("{MORE}{}", more(lines.len() - keep_n)),
        ));
    }
    if let Some(first) = out.first_mut() {
        first.starts = true;
    }
    out
}

/// `text` as [`prompt_rows`] draws it, dimmed: a line sent to the worker that
/// has not reached its transcript yet.
pub fn echo_rows(text: &str) -> Vec<Row> {
    let mut rows = prompt_rows(text, None);
    for s in rows.iter_mut().flat_map(|r| r.spans.iter_mut()) {
        s.tone = Tone::Echo;
    }
    rows
}

/// One line of a reply, its `` `code` `` and `**bold**` picked out.
fn inline(line: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut tone = Tone::Plain;
    let mut code = false;
    let mut buf = String::new();
    let mut chars = line.chars().peekable();
    let flush = |buf: &mut String, out: &mut Vec<Span>, tone: Tone| {
        if !buf.is_empty() {
            out.push(span(tone, std::mem::take(buf)));
        }
    };
    while let Some(c) = chars.next() {
        if c == '`' {
            flush(&mut buf, &mut out, if code { Tone::Code } else { tone });
            code = !code;
        } else if !code && c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            flush(&mut buf, &mut out, tone);
            tone = if tone == Tone::Strong {
                Tone::Plain
            } else {
                Tone::Strong
            };
        } else {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut out, if code { Tone::Code } else { tone });
    out
}

/// What the worker said: `● ` and its first line, the rest under it, with
/// light markdown (code, bold, headings) as Claude Code shows it.
fn said_rows(text: &str) -> Vec<Row> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.trim().lines().map(str::trim_end) {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if line.trim().is_empty() && !out.is_empty() {
            out.push(Row::default());
            continue;
        }
        let lead = if out.is_empty() { BULLET } else { MORE };
        let lead_tone = if out.is_empty() {
            Tone::Said
        } else {
            Tone::Plain
        };
        let mut spans = vec![span(lead_tone, lead)];
        if fenced {
            spans.push(span(Tone::Code, line));
        } else if let Some(h) = line
            .strip_prefix('#')
            .map(|h| h.trim_start_matches('#'))
            .and_then(|h| h.strip_prefix(' '))
        {
            spans.push(span(Tone::Strong, h));
        } else {
            spans.extend(inline(line));
        }
        out.push(Row::of(spans));
    }
    if let Some(first) = out.first_mut() {
        first.starts = true;
    }
    out
}

/// A tool's result: `  ⎿  ` and its first line, up to `keep` lines, then
/// `… +N lines`.
fn result_rows(text: &str, keep: usize, error: bool) -> Vec<Row> {
    let text = text
        .replace("<tool_use_error>", "")
        .replace("</tool_use_error>", "");
    let lines: Vec<&str> = text.trim_end().lines().map(str::trim_end).collect();
    let lines: Vec<&str> = lines
        .iter()
        .skip_while(|l| l.trim().is_empty())
        .copied()
        .collect();
    let tone = if error { Tone::Error } else { Tone::Plain };
    if lines.is_empty() {
        return vec![Row::of(vec![
            span(Tone::Plain, RESULT),
            span(Tone::Dim, "(No output)"),
        ])];
    }
    // Claude Code shows a result of up to one more line than it keeps
    // whole: `… +1 line` would hide nothing.
    let shown = if lines.len() > keep + 1 {
        keep
    } else {
        lines.len()
    };
    let mut out: Vec<Row> = lines
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, l)| {
            let lead = if i == 0 { RESULT } else { RESULT_MORE };
            Row::of(vec![span(Tone::Plain, lead), span(tone, clip(l, LINE_MAX))])
        })
        .collect();
    if lines.len() > shown {
        out.push(Row::of(vec![
            span(Tone::Plain, RESULT_MORE),
            span(Tone::Dim, more(lines.len() - shown)),
        ]));
    }
    out
}

/// Renders a transcript a line at a time. It remembers the calls whose
/// results are not shown (a `SubagentHandback`'s), so it is kept across
/// the lines of one transcript.
#[derive(Debug, Clone, Default)]
pub struct Render {
    hidden: HashSet<String>,
    sent_lines: usize,
}

impl Render {
    /// How many lines someone sent the worker it has rendered so far.
    pub fn sent_lines(&self) -> usize {
        self.sent_lines
    }

    /// Render one JSONL line as rows. Unknown or uninteresting lines
    /// render as nothing.
    pub fn line(&mut self, line: &str) -> Vec<Row> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let entry = |rows: Vec<Row>, out: &mut Vec<Row>| {
            let mut rows = rows.into_iter();
            if let Some(mut first) = rows.next() {
                first.starts = true;
                out.push(first);
            }
            out.extend(rows);
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(parts) = v.pointer("/message/content").and_then(Value::as_array) else {
                    return out;
                };
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                            entry(said_rows(text), &mut out);
                        }
                        // The final report: shown as what the worker said.
                        Some("tool_use") if str_of(part, "name") == Some("SubagentHandback") => {
                            if let Some(id) = str_of(part, "id") {
                                self.hidden.insert(id.to_string());
                            }
                            let msg = part.pointer("/input/message").and_then(Value::as_str);
                            entry(said_rows(msg.unwrap_or("")), &mut out);
                        }
                        Some("tool_use") => {
                            let (name, arg) = call_title(part);
                            let mut spans = vec![
                                span(Tone::Bullet(Call::Running), BULLET),
                                span(Tone::Strong, name),
                            ];
                            if !arg.is_empty() {
                                spans.push(span(Tone::Plain, format!("({arg})")));
                            }
                            let mut row = Row::of(spans);
                            row.call = str_of(part, "id").map(String::from);
                            entry(vec![row], &mut out);
                        }
                        // Thinking is private and often redacted; the tool calls
                        // around it say what it decided.
                        _ => {}
                    }
                }
            }
            Some("user") => match v.pointer("/message/content") {
                // A meta record is Claude Code talking to the worker, and
                // is left out, unless it carries a line someone sent: how a
                // message reaches a worker that `SendMessage` wakes.
                Some(Value::String(s)) => {
                    if v.get("isMeta").and_then(Value::as_bool) != Some(true) {
                        entry(prompt_rows(s, None), &mut out);
                    } else if sent(&v) {
                        self.sent_lines += 1;
                        entry(prompt_rows(unwrapped(s), None), &mut out);
                    }
                }
                Some(Value::Array(parts)) => {
                    let summary = v.get("toolUseResult").and_then(file_summary);
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                let id = str_of(part, "tool_use_id").unwrap_or("").to_string();
                                if self.hidden.contains(&id) {
                                    continue;
                                }
                                let error =
                                    part.get("is_error").and_then(Value::as_bool) == Some(true);
                                let text = match (&summary, error) {
                                    (Some(s), false) => s.clone(),
                                    _ => result_text(part.get("content").unwrap_or(&Value::Null)),
                                };
                                let mut rows = result_rows(&text, RESULT_LINES, error);
                                rows[0].answers = Some((id, error));
                                out.extend(rows);
                            }
                            Some("text") => {
                                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                                entry(prompt_rows(text, None), &mut out);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            // A message to a running worker is not a `user` line: it lands as
            // a queued command attachment when the worker takes it, origin
            // human when typed in its own Claude Code view, coordinator when
            // its parent sent it (`SendMessage`).
            Some("attachment") => {
                let att = v.get("attachment").unwrap_or(&Value::Null);
                if att.get("type").and_then(Value::as_str) == Some("queued_command")
                    && sent(att)
                    && let Some(text) = att.get("prompt").and_then(Value::as_str)
                {
                    self.sent_lines += 1;
                    entry(prompt_rows(text, None), &mut out);
                }
            }
            _ => {}
        }
        out
    }
}

/// A record whose origin says a person typed it or the worker's parent sent
/// it.
fn sent(record: &Value) -> bool {
    matches!(
        record.pointer("/origin/kind").and_then(Value::as_str),
        Some("human" | "coordinator")
    )
}

/// The line inside Claude Code's wrapper. Text in no wrapper it is known to
/// use is returned whole, so a change of its words loses nothing.
fn unwrapped(text: &str) -> &str {
    SENT_WRAPPERS
        .iter()
        .find_map(|(head, tail)| text.strip_prefix(head)?.strip_suffix(tail))
        .unwrap_or(text)
}

/// Rows as lines of text, with a blank line before each entry but the
/// first: how the page lays them out.
pub fn lay_out(rows: &[Row], first: bool, style: Style) -> Vec<String> {
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.starts && !(first && i == 0) {
            out.push(String::new());
        }
        out.push(r.to_ansi(style));
    }
    out
}

/// What [`Tail::read`] found since the last read.
#[derive(Debug, Default)]
pub struct Update {
    /// The file shrank or was replaced: what was read before is void.
    pub restarted: bool,
    /// Whole JSONL lines appended since the last read.
    pub lines: Vec<String>,
}

/// Reads a growing JSONL file a whole line at a time: the `tail -f` under
/// the worker page's live view.
#[derive(Debug, Default)]
pub struct Tail {
    offset: u64,
    pending: String,
}

impl Tail {
    /// Read what was appended since the last call. A half-written last line
    /// is held back until its newline arrives.
    pub fn read(&mut self, path: &Path) -> std::io::Result<Update> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut update = Update::default();
        if len < self.offset {
            update.restarted = true;
            self.offset = 0;
            self.pending.clear();
        }
        if len > self.offset {
            file.seek(SeekFrom::Start(self.offset))?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(len - self.offset)
                .read_to_end(&mut bytes)?;
            self.offset += bytes.len() as u64;
            self.pending.push_str(&String::from_utf8_lossy(&bytes));
            if let Some(cut) = self.pending.rfind('\n') {
                let complete: String = self.pending.drain(..=cut).collect();
                update.lines = complete.lines().map(str::to_string).collect();
            }
        }
        Ok(update)
    }

    /// The held-back last line, if the file ends without a newline — for a
    /// read that will not wait for it.
    pub fn rest(&self) -> Option<&str> {
        Some(self.pending.as_str()).filter(|p| !p.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Style = Style { color: false };

    /// A transcript's lines rendered as the worker page lays them out, plain.
    fn view(lines: &[&str]) -> Vec<String> {
        let mut r = Render::default();
        let mut rows: Vec<Row> = Vec::new();
        for l in lines {
            push(&mut rows, r.line(l));
        }
        lay_out(&rows, true, PLAIN)
    }

    /// A test worker as Claude Code 2.1.304 wrote it, and as its subagent
    /// view (ctrl+o) showed it.
    #[test]
    fn a_worker_reads_as_claude_codes_view_of_it() {
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-20T11:55:01Z","message":{"role":"user","content":"Read notes.txt. Then run seq 1 40 with Bash."}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>\nx\n</system-reminder>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/tmp/w/proj/notes.txt"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"seq 1 40","description":"Print numbers 1 through 40"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"r1","type":"tool_result","content":"1\tThe pier.\n2\tSeven planks.\n3\tLilies.\n4\t"}]},"toolUseResult":{"type":"text","file":{"filePath":"/tmp/w/proj/notes.txt","numLines":4,"startLine":1,"totalLines":4}}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"b1","type":"tool_result","content":"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30\n31\n32\n33\n34\n35\n36\n37\n38\n39\n40","is_error":false}]},"toolUseResult":{"stdout":"1\n2","stderr":""}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"h1","name":"SubagentHandback","input":{"message":"notes.txt has 3 lines.\nRan `seq 1 40`."}}]}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"h1","type":"tool_result","content":[{"type":"text","text":"{\"success\":true}"}]}]}}"#,
        ];
        assert_eq!(
            view(&lines),
            vec![
                "❯ Read notes.txt. Then run seq 1 40 with Bash.",
                "",
                "● Read(/tmp/w/proj/notes.txt)",
                "  ⎿  Read 4 lines",
                "",
                "● Bash(seq 1 40)",
                "  ⎿  1",
                "     2",
                "     3",
                "     … +37 lines",
                "",
                "● notes.txt has 3 lines.",
                "  Ran seq 1 40.",
            ]
        );
    }

    /// A line passed on by the worker's parent (`SendMessage`) while the
    /// worker runs lands as a queued command, as one typed in its view does
    /// (Claude Code 2.1.317). Other queued commands stay out.
    #[test]
    fn a_message_queued_for_a_running_worker_is_its_prompt() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"Sleep, then report."}}"#,
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"PING-ONE","origin":{"kind":"coordinator"},"isMeta":true}}"#,
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"typed in its view","origin":{"kind":"human"}}}"#,
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"a task notification","origin":{"kind":"task-notification"}}}"#,
        ];
        assert_eq!(
            view(&lines),
            vec![
                "❯ Sleep, then report.",
                "",
                "❯ PING-ONE",
                "",
                "❯ typed in its view",
            ]
        );
    }

    /// A worker `SendMessage` wakes after its turn ended gets the line as a
    /// meta `user` record instead, in Claude Code's words (Claude Code
    /// 2.1.317). The page shows the line alone, and every other meta record
    /// stays out.
    #[test]
    fn a_message_that_wakes_a_finished_worker_is_its_prompt() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"Count, then report."}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"coordinator"},"message":{"role":"user","content":"The coordinator sent a message while you were working:\ntestingggggggggggggg\n\nAddress this before completing your current task."}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"coordinator"},"message":{"role":"user","content":"The coordinator sent a message while you were working:\nfirst\n\nsecond\n\nAddress this before completing your current task."}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"role":"user","content":"The user sent a new message while you were working:\ntyped in its view\n\nThis is how Claude Code surfaces messages the user sends mid-turn \u2014 within the running turn, often alongside the next tool result, rather than as a separate conversation turn. Address the message above as you continue this turn."}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>\nx\n</system-reminder>"}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"task-notification"},"message":{"role":"user","content":"[SYSTEM NOTIFICATION - NOT USER INPUT] a task ended"}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"peer"},"message":{"role":"user","content":"Another Claude session sent a message"}}"#,
        ];
        assert_eq!(
            view(&lines),
            vec![
                "❯ Count, then report.",
                "",
                "❯ testingggggggggggggg",
                "",
                "❯ first",
                "  ",
                "  second",
                "",
                "❯ typed in its view",
            ]
        );
    }

    /// Both shapes count; a worker's first prompt and other records don't.
    #[test]
    fn it_counts_the_lines_sent_to_a_worker() {
        let mut r = Render::default();
        for l in [
            r#"{"type":"user","message":{"role":"user","content":"Count, then report."}}"#,
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"a task notification","origin":{"kind":"task-notification"}}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"peer"},"message":{"role":"user","content":"Another Claude session sent a message"}}"#,
        ] {
            r.line(l);
        }
        assert_eq!(r.sent_lines(), 0);
        r.line(
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"PING","origin":{"kind":"coordinator"}}}"#,
        );
        r.line(
            r#"{"type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"role":"user","content":"hello"}}"#,
        );
        assert_eq!(r.sent_lines(), 2);
    }

    /// The shape of the prompt it stands in for, in its own tone, so the real
    /// row takes its place with nothing moving.
    #[test]
    fn an_echo_is_a_prompt_dimmed() {
        let echo = echo_rows("first\n\nsecond");
        let prompt = prompt_rows("first\n\nsecond", None);
        assert_eq!(
            echo.iter().map(Row::plain).collect::<Vec<_>>(),
            prompt.iter().map(Row::plain).collect::<Vec<_>>()
        );
        assert!(echo[0].starts);
        assert!(echo.iter().all(|r| r.band() == Some(Tone::Echo)));
        assert!(prompt.iter().all(|r| r.band() == Some(Tone::Prompt)));
        let on = Style { color: true };
        assert!(
            echo[0]
                .to_ansi(on)
                .starts_with("\x1b[2m\x1b[48;2;55;55;55m")
        );
    }

    /// Words Claude Code may change to: the line is shown with them rather
    /// than lost.
    #[test]
    fn a_sent_line_in_an_unknown_wrapper_is_shown_whole() {
        for text in [
            "The coordinator now says:\nhello",
            "The coordinator sent a message while you were working:\nhello",
            "hello\n\nAddress this before completing your current task.",
        ] {
            assert_eq!(unwrapped(text), text);
        }
    }

    #[test]
    fn tail_reads_whole_lines_and_notices_a_restart() {
        let dir = std::env::temp_dir().join(format!("giverny-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-t.jsonl");
        std::fs::write(&path, "a\nb").unwrap();
        let mut tail = Tail::default();
        let u = tail.read(&path).unwrap();
        assert_eq!(u.lines, vec!["a"]);
        assert_eq!(tail.rest(), Some("b"));
        std::fs::write(&path, "a\nbc\nd\n").unwrap();
        let u = tail.read(&path).unwrap();
        assert!(!u.restarted);
        assert_eq!(u.lines, vec!["bc", "d"]);
        assert!(tail.read(&path).unwrap().lines.is_empty());
        std::fs::write(&path, "z\n").unwrap();
        let u = tail.read(&path).unwrap();
        assert!(u.restarted);
        assert_eq!(u.lines, vec!["z"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
