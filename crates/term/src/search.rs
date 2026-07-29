//! Scrollback search and click targets (paths, URLs, OSC 8 hyperlinks).

use std::ops::RangeInclusive;
use std::path::PathBuf;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::term::search::{Match, RegexSearch};

/// One search session over a terminal's scrollback.
pub struct Search {
    pub query: String,
    pub needs_focus: bool,
    regex: Option<RegexSearch>,
    /// Where the next `find` starts from.
    origin: Option<Point>,
    pub current: Option<Match>,
    pub no_match: bool,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            query: String::new(),
            needs_focus: true,
            regex: None,
            origin: None,
            current: None,
            no_match: false,
        }
    }
}

impl Search {
    /// Rebuild the matcher after the query changed. Literal by default —
    /// terminal output is full of regex metacharacters.
    pub fn set_query(&mut self, query: String) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.origin = None;
        self.current = None;
        self.no_match = false;
        self.regex = if self.query.is_empty() {
            None
        } else {
            RegexSearch::new(&regex_syntax::escape(&self.query)).ok()
        };
    }

    /// Find the next match in `direction`, scroll it into view, and select it.
    /// Wraps around the buffer ends.
    pub fn find<T: EventListener>(&mut self, term: &mut Term<T>, direction: Direction) {
        let Some(regex) = &mut self.regex else { return };
        let display_offset = term.grid().display_offset();
        let last_line = term.grid().screen_lines() as i32 - 1;
        let bottom = Point::new(Line(last_line), Column(term.grid().columns() - 1));

        // Start from the last hit (stepping past it), else the viewport edge.
        let origin = match (self.origin, direction) {
            (Some(p), Direction::Right) => term.grid().iter_from(p).next().map(|c| c.point),
            (Some(p), Direction::Left) => Some(p.sub(term, Boundary::Grid, 1)),
            (None, Direction::Right) => Some(Point::new(Line(-(display_offset as i32)), Column(0))),
            (None, Direction::Left) => Some(bottom),
        }
        .unwrap_or(bottom);

        let hit = term
            .search_next(regex, origin, direction, Side::Left, None)
            .or_else(|| {
                // Wrap: restart from the far end of the buffer.
                let history = term.grid().total_lines() - term.grid().screen_lines();
                let wrap_from = match direction {
                    Direction::Right => Point::new(Line(-(history as i32)), Column(0)),
                    Direction::Left => bottom,
                };
                term.search_next(regex, wrap_from, direction, Side::Left, None)
            });

        match hit {
            Some(m) => {
                let point = *m.start();
                self.origin = Some(point);
                self.current = Some(m);
                self.no_match = false;
                term.scroll_to_point(point);
            }
            None => {
                self.no_match = true;
                self.current = None;
            }
        }
    }

    /// Viewport rows (0-based) covered by the current match, for highlighting.
    pub fn highlight_rows<T: EventListener>(&self, term: &Term<T>) -> Option<RangeInclusive<i32>> {
        let m = self.current.as_ref()?;
        let offset = term.grid().display_offset() as i32;
        Some((m.start().line.0 + offset)..=(m.end().line.0 + offset))
    }

    pub fn clear<T: EventListener>(&self, term: &mut Term<T>) {
        term.scroll_display(Scroll::Bottom);
    }
}

/// Something clickable under the pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    Url(String),
    /// Local path plus an optional `:line` suffix (Claude prints these).
    File(PathBuf, Option<u32>),
}

impl ClickTarget {
    /// Open with the platform handler; files honor `$EDITOR` when it is a
    /// terminal editor the user can see (else the desktop handler).
    pub fn open(&self) {
        match self {
            ClickTarget::Url(url) => {
                let _ = open::that_detached(url);
            }
            ClickTarget::File(path, _) => {
                let _ = open::that_detached(path);
            }
        }
    }
}

const URL_SCHEMES: [&str; 4] = ["http://", "https://", "file://", "ftp://"];
/// Characters that terminate a path/URL token in terminal output.
const TOKEN_STOPS: [char; 12] = [' ', '\t', '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<'];

/// Extract the click target inside `text` at byte-ish char index `col`.
/// `cwd` resolves relative paths (Claude prints `src/main.rs:42`).
pub fn target_at(text: &str, col: usize, cwd: &std::path::Path) -> Option<ClickTarget> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let is_stop = |c: char| TOKEN_STOPS.contains(&c) || c.is_control();
    let start = (0..=col).rev().take_while(|&i| !is_stop(chars[i])).last()?;
    let end = (col..chars.len())
        .take_while(|&i| !is_stop(chars[i]))
        .last()?;
    let token: String = chars[start..=end].iter().collect();
    // Trailing punctuation is prose, not part of the target.
    let token = token.trim_end_matches([',', '.', ';', ':', '!', '?']);
    if token.is_empty() {
        return None;
    }

    if URL_SCHEMES.iter().any(|s| token.starts_with(s)) {
        return Some(ClickTarget::Url(token.to_string()));
    }
    if token.starts_with("www.") {
        return Some(ClickTarget::Url(format!("https://{token}")));
    }

    // path[:line[:col]]
    let mut parts = token.split(':');
    let raw_path = parts.next()?;
    let line: Option<u32> = parts.next().and_then(|p| p.parse().ok());
    let expanded = if let Some(rest) = raw_path.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(raw_path)
    };
    let full = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    if full.exists() {
        Some(ClickTarget::File(full, line))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls() {
        let cwd = std::env::temp_dir();
        let line = "see https://example.com/x?a=1 for details";
        assert_eq!(
            target_at(line, 10, &cwd),
            Some(ClickTarget::Url("https://example.com/x?a=1".into()))
        );
        assert_eq!(
            target_at(line, 0, &cwd),
            None,
            "plain words are not targets"
        );
    }

    #[test]
    fn strips_trailing_prose_punctuation() {
        let cwd = std::env::temp_dir();
        assert_eq!(
            target_at("visit https://example.com.", 12, &cwd),
            Some(ClickTarget::Url("https://example.com".into()))
        );
    }

    #[test]
    fn finds_existing_files_with_line_numbers() {
        let dir = std::env::temp_dir().join(format!("giverny-click-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main() {}").unwrap();

        // Relative, with a :line suffix — the shape Claude prints.
        let hit = target_at("edited src/main.rs:42 ok", 8, &dir);
        assert_eq!(
            hit,
            Some(ClickTarget::File(dir.join("src/main.rs"), Some(42)))
        );

        // Absolute path.
        let abs = format!("{}", dir.join("src/main.rs").display());
        let text = format!("see {abs} now");
        assert!(matches!(
            target_at(&text, 6, &dir),
            Some(ClickTarget::File(..))
        ));

        // Paths that do not exist are not click targets.
        assert_eq!(target_at("nope src/ghost.rs here", 8, &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quotes_and_brackets_bound_the_token() {
        let dir = std::env::temp_dir().join(format!("giverny-click2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        let text = "(\"f.txt\")";
        assert_eq!(
            target_at(text, 3, &dir),
            Some(ClickTarget::File(dir.join("f.txt"), None))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
