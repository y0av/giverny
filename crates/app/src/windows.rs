//! Windows beyond the first.
//!
//! Every window shows the same workspace: one rail listing every row, each
//! window with its own current row. Drawing a window swaps its focus (and
//! the few things that are per window: where its terminals and rows are on
//! screen, its titlebar's maximise state) into the `App`, runs the same
//! frame code the first window runs, and swaps back. So a second window
//! gets every tab operation the first has, without a second copy of them.
//!
//! A row is on screen in at most one window: a terminal has one size, and
//! two windows fighting over it would resize it every frame. Choosing a row
//! in one window takes it from any other ([`settle`]).

use eframe::egui;
use giverny_core::panes::Area;
use giverny_core::state::WindowSave;
use giverny_core::tabs::{Focus, TabId, Workspace};

use crate::titlebar;

/// A window other than the first.
pub struct Extra {
    /// Stable for the window's life: its viewport id is made from it.
    pub key: u64,
    /// Parked here while another window is drawn; the first window's while
    /// this one is.
    pub focus: Focus,
    pub size: Option<[f32; 2]>,
    pub rail_hidden: bool,
    pub row_rects: Vec<(egui::Rect, TabId)>,
    pub pane_rects: Vec<(egui::Rect, TabId)>,
    pub pane_areas: Vec<(TabId, Area)>,
    pub focus_terminal: bool,
    pub maximize: titlebar::Maximize,
    /// The window's close button was pressed.
    pub closing: bool,
    /// Done in this window the first time it is drawn: what it opened for.
    pub pending: Vec<crate::Action>,
    /// Documentation capture of this window (`GIVERNY_CAPTURE`).
    pub capture: Option<crate::capture::Capture>,
}

impl Extra {
    pub fn new(key: u64, active: Option<TabId>) -> Self {
        Extra {
            key,
            focus: Focus {
                active,
                recent: Vec::new(),
            },
            size: None,
            rail_hidden: false,
            row_rects: Vec::new(),
            pane_rects: Vec::new(),
            pane_areas: Vec::new(),
            focus_terminal: true,
            maximize: titlebar::Maximize::default(),
            closing: false,
            pending: Vec::new(),
            capture: crate::capture::Capture::for_window(key as usize + 1),
        }
    }

    pub fn restore(key: u64, saved: &WindowSave) -> Self {
        Extra {
            focus: saved.focus.clone(),
            size: saved.size,
            rail_hidden: saved.rail_hidden,
            ..Extra::new(key, None)
        }
    }

    pub fn save(&self) -> WindowSave {
        WindowSave {
            focus: self.focus.clone(),
            size: self.size,
            rail_hidden: self.rail_hidden,
        }
    }

    pub fn viewport_id(&self) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(("giverny-window", self.key))
    }
}

/// Make the windows agree after the one being drawn (whose focus is in
/// `ws`) chose its row: any other showing that row falls back to a row of
/// its own past nobody else shows, or to nothing. A window whose terminal
/// has closed falls back the same way.
pub fn settle(ws: &Workspace, parked: &mut [&mut Focus]) {
    let mut taken: Vec<TabId> = ws.active.into_iter().collect();
    for focus in parked.iter_mut() {
        let focus: &mut Focus = focus;
        let gone = focus.active.is_some_and(|a| ws.tab(a).is_none());
        let clash = focus
            .active
            .is_some_and(|a| taken.iter().any(|&t| ws.same_row(t, a)));
        if gone || clash {
            let next = focus
                .recent
                .iter()
                .copied()
                .filter(|&t| ws.tab(t).is_some())
                .find(|&t| !taken.iter().any(|&x| ws.same_row(x, t)))
                .map(|t| ws.row_target(t));
            focus
                .recent
                .retain(|&t| ws.tab(t).is_some() && Some(t) != next);
            focus.active = next;
        }
        if let Some(a) = focus.active {
            taken.push(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use giverny_core::panes::Axis;

    #[test]
    fn a_row_chosen_here_leaves_the_other_window() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        // The other window showed b, and c before that.
        let mut other = Focus {
            active: Some(b),
            recent: vec![c, a],
        };
        ws.set_active(b);
        settle(&ws, &mut [&mut other]);
        assert_eq!(other.active, Some(c), "back to its own last row");

        // Nothing of its own left that is free: it shows nothing.
        let mut other = Focus {
            active: Some(b),
            recent: vec![],
        };
        settle(&ws, &mut [&mut other]);
        assert_eq!(other.active, None);
    }

    #[test]
    fn a_split_counts_as_one_row_across_windows() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let x = ws.add_tab(cat);
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let mut other = Focus {
            active: Some(a),
            recent: vec![x],
        };
        ws.set_active(b);
        settle(&ws, &mut [&mut other]);
        assert_eq!(other.active, Some(x));
    }

    #[test]
    fn a_closed_terminal_leaves_its_window_on_the_next_row() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        ws.set_active(a);
        let mut other = Focus {
            active: Some(c),
            recent: vec![b],
        };
        ws.close_tab(c);
        settle(&ws, &mut [&mut other]);
        assert_eq!(other.active, Some(b));
    }
}
