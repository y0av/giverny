//! Right-to-left text, reordered for display.
//!
//! The grid stores characters in *logical* order — the order they were typed
//! or written — which is correct and is what the program on the other end
//! expects to read back. Drawing them in that order puts Hebrew and Arabic on
//! screen backwards, because RTL script runs right to left. VTE (and so GNOME
//! Terminal) applies the Unicode Bidi Algorithm per line before drawing; this
//! is the same idea.
//!
//! Display only: nothing here touches the grid. A row with no RTL character
//! never reaches the algorithm, so ordinary Latin output costs one cheap scan.

use unicode_bidi::{BidiInfo, Level};

/// Could this character start an RTL run?
///
/// A range test, not a table lookup: this runs per cell on every frame, and
/// its only job is to decide whether the real algorithm is worth running.
/// Covers Hebrew, Arabic, Syriac, Thaana, N'Ko, Samaritan, Mandaic and the
/// Arabic presentation forms.
pub fn is_rtl(c: char) -> bool {
    matches!(c,
        '\u{0590}'..='\u{08FF}'
        | '\u{FB1D}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFF}'
        | '\u{10800}'..='\u{10FFF}'
        | '\u{1E800}'..='\u{1EFFF}'
    )
}

pub fn row_has_rtl(chars: &[char]) -> bool {
    chars.iter().copied().any(is_rtl)
}

/// Where each logical cell of a row should be drawn: `out[logical] = visual`.
///
/// The paragraph direction is forced LTR. A terminal line starts at the left
/// edge whatever it contains — a prompt does not move to the right margin
/// because the output after it happens to be Hebrew.
pub fn logical_to_visual(chars: &[char]) -> Vec<usize> {
    let text: String = chars.iter().collect();
    let info = BidiInfo::new(&text, Some(Level::ltr()));

    // `BidiInfo::levels` is indexed by *byte*; a cell is a char. Take the
    // level at each char's byte offset to get one level per cell.
    let mut levels = Vec::with_capacity(chars.len());
    for (offset, _) in text.char_indices() {
        levels.push(info.levels[offset]);
    }

    let visual_to_logical = BidiInfo::reorder_visual(&levels);
    let mut out = vec![0usize; chars.len()];
    for (visual, &logical) in visual_to_logical.iter().enumerate() {
        // Defensive: a mismatch would otherwise panic on a user's screenful of
        // text. Dropping to identity is wrong-looking, not fatal.
        if logical < out.len() {
            out[logical] = visual;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Read a row back the way it would be drawn.
    fn drawn(s: &str) -> String {
        let cs = chars(s);
        let map = logical_to_visual(&cs);
        let mut out = vec![' '; cs.len()];
        for (logical, &visual) in map.iter().enumerate() {
            out[visual] = cs[logical];
        }
        out.into_iter().collect()
    }

    #[test]
    fn latin_is_left_exactly_as_it_was() {
        assert_eq!(drawn("cargo build --release"), "cargo build --release");
        assert!(!row_has_rtl(&chars("cargo build")), "no algorithm needed");
    }

    #[test]
    fn hebrew_is_reversed_for_display() {
        // Logical order is what the program wrote; visual order is right-to-left.
        let word = "שלום";
        assert!(row_has_rtl(&chars(word)));
        assert_eq!(drawn(word), word.chars().rev().collect::<String>());
    }

    #[test]
    fn mixed_lines_reorder_only_the_rtl_run() {
        // The Latin words keep their order and their places; only the Hebrew
        // run flips. This is the case that a naive "reverse the line" gets
        // wrong, and the reason for using the real algorithm.
        let drawn = drawn("git commit שלום now");
        assert!(drawn.starts_with("git commit "), "{drawn}");
        assert!(drawn.ends_with(" now"), "{drawn}");
        assert!(drawn.contains(&"שלום".chars().rev().collect::<String>()));
    }

    #[test]
    fn numbers_inside_hebrew_stay_readable() {
        // The classic bidi case: digits are left-to-right even inside an RTL
        // run, so "12" must not come out as "21".
        let d = drawn("שלום 12 עולם");
        assert!(d.contains("12"), "digits reversed: {d}");
    }

    #[test]
    fn every_cell_gets_exactly_one_place() {
        // A permutation, or cells would overwrite each other on screen.
        for s in ["שלום", "abc שלום def", "a", "", "שלום 12 עולם abc"] {
            let cs = chars(s);
            let map = logical_to_visual(&cs);
            assert_eq!(map.len(), cs.len());
            let mut seen = map.clone();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), cs.len(), "not a permutation for {s:?}");
            assert!(map.iter().all(|&v| v < cs.len().max(1)) || cs.is_empty());
        }
    }

    #[test]
    fn trailing_blanks_do_not_drag_the_text_right() {
        // Terminal rows are padded with spaces to the full width; the text has
        // to stay at the left edge regardless.
        let d = drawn("שלום      ");
        assert!(!d.starts_with(' '), "text pushed away from the left: {d:?}");
    }
}
