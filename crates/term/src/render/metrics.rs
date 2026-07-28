//! Font discovery (fontdb) and integer physical-pixel cell metrics.
//!
//! Cell metrics are always whole pixels: every glyph origin lands on the
//! pixel grid, which removes subpixel positioning from the atlas key and
//! avoids background-seam shimmer between cells.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fontdb::{Database, Family, Query, Source as DbSource, Stretch, Style, Weight};
use swash::FontRef;

/// Synthetic style bits carried into the atlas key.
pub const SYNTH_BOLD: u8 = 1 << 0;
pub const SYNTH_ITALIC: u8 = 1 << 1;

/// Default primary family candidates, tried in order before generic monospace.
const PRIMARY_CANDIDATES: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "DejaVu Sans Mono",
];

/// Fallback faces appended after the primary family styles.
const FALLBACK_CANDIDATES: &[&str] = &[
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "PowerlineSymbols",
    "Noto Color Emoji",
    "Noto Emoji",
    "DejaVu Sans Mono",
    "DejaVu Sans",
];

/// One loaded font face (owned bytes + collection index).
pub struct LoadedFont {
    pub family: String,
    data: Arc<Vec<u8>>,
    index: u32,
}

impl LoadedFont {
    pub fn as_font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
    }
}

/// A glyph resolution: which loaded face, which glyph, which synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub slot: u16,
    pub glyph: u16,
    pub synth: u8,
}

/// Resolved set of faces. Slot 0 is the primary regular face; real
/// bold/italic/bold-italic faces (when the family has them) plus fallbacks
/// follow. Not `Sync`: lives on the UI thread.
pub struct FontSet {
    fonts: Vec<LoadedFont>,
    bold: Option<u16>,
    italic: Option<u16>,
    bold_italic: Option<u16>,
    /// Lazy charmap memo: (slot, char) → glyph id (0 = absent).
    charmap_cache: RefCell<HashMap<(u16, char), u16>>,
}

impl FontSet {
    /// Load fonts from the system database. `preferred` (from config) is
    /// tried before the built-in candidates.
    pub fn load(preferred: Option<&str>) -> anyhow::Result<FontSet> {
        let mut db = Database::new();
        db.load_system_fonts();
        Self::load_from_db(&db, preferred)
    }

    fn load_from_db(db: &Database, preferred: Option<&str>) -> anyhow::Result<FontSet> {
        let mut families: Vec<Family<'_>> = Vec::new();
        if let Some(name) = preferred {
            families.push(Family::Name(name));
        }
        families.extend(PRIMARY_CANDIDATES.iter().map(|n| Family::Name(n)));
        families.push(Family::Monospace);

        let query = |weight: Weight, style: Style, fams: &[Family<'_>]| {
            db.query(&Query {
                families: fams,
                weight,
                stretch: Stretch::Normal,
                style,
            })
        };

        let regular_id = query(Weight::NORMAL, Style::Normal, &families)
            .ok_or_else(|| anyhow::anyhow!("no usable monospace font found on this system"))?;

        let mut seen: HashSet<fontdb::ID> = HashSet::new();
        let mut fonts: Vec<LoadedFont> = Vec::new();

        let regular = push_face(db, regular_id, &mut seen, &mut fonts)
            .ok_or_else(|| anyhow::anyhow!("failed to load font data for primary face"))?;
        debug_assert_eq!(regular, 0);

        // Real styled faces of the *resolved* primary family only.
        let primary_family = fonts[0].family.clone();
        let prim = [Family::Name(primary_family.as_str())];
        let bold = query(Weight::BOLD, Style::Normal, &prim)
            .and_then(|id| push_face(db, id, &mut seen, &mut fonts));
        let italic = query(Weight::NORMAL, Style::Italic, &prim)
            .and_then(|id| push_face(db, id, &mut seen, &mut fonts));
        let bold_italic = query(Weight::BOLD, Style::Italic, &prim)
            .and_then(|id| push_face(db, id, &mut seen, &mut fonts));

        for name in FALLBACK_CANDIDATES {
            let fams = [Family::Name(name)];
            if let Some(id) = query(Weight::NORMAL, Style::Normal, &fams) {
                push_face(db, id, &mut seen, &mut fonts);
            }
        }

        Ok(FontSet {
            fonts,
            bold,
            italic,
            bold_italic,
            charmap_cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn font(&self, slot: u16) -> &LoadedFont {
        &self.fonts[slot as usize]
    }

    pub fn primary(&self) -> &LoadedFont {
        &self.fonts[0]
    }

    fn glyph_in(&self, slot: u16, ch: char) -> u16 {
        if let Some(&g) = self.charmap_cache.borrow().get(&(slot, ch)) {
            return g;
        }
        let g = self.fonts[slot as usize]
            .as_font()
            .map(|f| f.charmap().map(ch))
            .unwrap_or(0);
        self.charmap_cache.borrow_mut().insert((slot, ch), g);
        g
    }

    /// Resolve a character + style to a face slot, glyph id and synthesis
    /// flags. Walks: styled primary face → primary regular (with synthesis)
    /// → fallbacks (with synthesis).
    pub fn resolve(&self, ch: char, bold: bool, italic: bool) -> Option<Resolved> {
        let styled_slot = match (bold, italic) {
            (true, true) => self.bold_italic.or(self.bold).or(self.italic),
            (true, false) => self.bold,
            (false, true) => self.italic,
            (false, false) => None,
        };
        if let Some(slot) = styled_slot {
            let glyph = self.glyph_in(slot, ch);
            if glyph != 0 {
                // A real bold face still needs italic synth when we only had
                // bold (and vice versa) — compute what the face lacks.
                let mut synth = 0;
                if bold && Some(slot) == self.italic {
                    synth |= SYNTH_BOLD;
                }
                if italic && Some(slot) == self.bold {
                    synth |= SYNTH_ITALIC;
                }
                return Some(Resolved { slot, glyph, synth });
            }
        }

        let mut synth = 0;
        if bold {
            synth |= SYNTH_BOLD;
        }
        if italic {
            synth |= SYNTH_ITALIC;
        }
        for slot in std::iter::once(0u16).chain((0..self.fonts.len() as u16).filter(|s| *s != 0)) {
            let glyph = self.glyph_in(slot, ch);
            if glyph != 0 {
                return Some(Resolved { slot, glyph, synth });
            }
        }
        None
    }
}

fn push_face(
    db: &Database,
    id: fontdb::ID,
    seen: &mut HashSet<fontdb::ID>,
    fonts: &mut Vec<LoadedFont>,
) -> Option<u16> {
    if !seen.insert(id) {
        return None;
    }
    let face = db.face(id)?;
    let family = face
        .families
        .first()
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    let font = load_face(db, id, family)?;
    fonts.push(font);
    Some((fonts.len() - 1) as u16)
}

fn load_face(db: &Database, id: fontdb::ID, family: String) -> Option<LoadedFont> {
    let (source, index) = db.face_source(id)?;
    let data: Arc<Vec<u8>> = match source {
        DbSource::Binary(bin) => Arc::new(bin.as_ref().as_ref().to_vec()),
        DbSource::File(path) => Arc::new(std::fs::read(path).ok()?),
        DbSource::SharedFile(_, bin) => Arc::new(bin.as_ref().as_ref().to_vec()),
    };
    // Validate the face parses before accepting it.
    FontRef::from_index(&data, index as usize)?;
    Some(LoadedFont {
        family,
        data,
        index,
    })
}

/// Integer physical-pixel cell geometry for a font at a pixel size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub cell_w: u32,
    pub cell_h: u32,
    /// Baseline distance from the cell top, in pixels.
    pub baseline: u32,
    /// The exact ppem used for rasterization.
    pub ppem: f32,
}

impl CellMetrics {
    pub fn compute(font: FontRef<'_>, px: f32) -> CellMetrics {
        let m = font.metrics(&[]).scale(px);
        let gm = font.glyph_metrics(&[]).scale(px);
        let reference = font.charmap().map('M');
        let advance = if reference != 0 {
            gm.advance_width(reference)
        } else {
            px * 0.6
        };
        let cell_w = advance.round().max(1.0) as u32;
        let cell_h = (m.ascent + m.descent + m.leading).ceil().max(1.0) as u32;
        let baseline = (m.ascent + m.leading / 2.0).round().min(cell_h as f32) as u32;
        CellMetrics {
            cell_w,
            cell_h,
            baseline,
            ppem: px,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> FontSet {
        FontSet::load(None).expect("system should have a monospace font")
    }

    #[test]
    fn loads_primary_and_metrics_are_integer() {
        let s = set();
        let font = s.primary().as_font().expect("parse primary");
        let m = CellMetrics::compute(font, 15.0);
        assert!(m.cell_w >= 4 && m.cell_w <= 40, "cell_w sane: {}", m.cell_w);
        assert!(m.cell_h >= 8 && m.cell_h <= 60, "cell_h sane: {}", m.cell_h);
        assert!(m.baseline > 0 && m.baseline <= m.cell_h);
    }

    #[test]
    fn resolves_ascii_and_caches() {
        let s = set();
        let r = s.resolve('A', false, false).expect("A must resolve");
        assert_eq!(r.slot, 0);
        assert_ne!(r.glyph, 0);
        assert_eq!(r.synth, 0);
        let r2 = s.resolve('A', false, false).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn bold_resolves_with_face_or_synth() {
        let s = set();
        let r = s.resolve('B', true, false).expect("bold B must resolve");
        assert_ne!(r.glyph, 0);
        if r.slot == 0 {
            assert_eq!(
                r.synth & SYNTH_BOLD,
                SYNTH_BOLD,
                "regular face ⇒ synthetic bold"
            );
        }
    }

    #[test]
    fn box_drawing_resolves_somewhere() {
        let s = set();
        // Claude Code UI uses box drawing + braille spinners heavily.
        for ch in ['─', '│', '╭', '⠋'] {
            assert!(s.resolve(ch, false, false).is_some(), "no font for {ch:?}");
        }
    }
}
