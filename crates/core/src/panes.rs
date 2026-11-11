//! Split layouts: how the terminals of one rail row share the space.
//!
//! A binary tree, bspwm style. Each split divides its rectangle in two along
//! one axis at `ratio`, and each leaf is a terminal. Pure geometry, no UI, so
//! the operations unit-test without a window.

use serde::{Deserialize, Serialize};

use crate::tabs::TabId;

/// Which way a split lays its two halves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Side by side: `a` on the left, `b` on the right.
    Row,
    /// Stacked: `a` on top, `b` below.
    Column,
}

/// One of the four ways to move between terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    Leaf(TabId),
    Split {
        axis: Axis,
        /// The share of the space `a` gets, 0..1.
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// The smallest share a drag may leave either side of a divider.
pub const MIN_RATIO: f32 = 0.1;

/// A rectangle in whatever units the caller lays out in: `[x, y, w, h]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Area { x, y, w, h }
    }

    fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Split along `axis` at `ratio`, leaving `gap` between the halves.
    fn split(&self, axis: Axis, ratio: f32, gap: f32) -> (Area, Area) {
        match axis {
            Axis::Row => {
                let room = (self.w - gap).max(0.0);
                let wa = (room * ratio).round();
                (
                    Area::new(self.x, self.y, wa, self.h),
                    Area::new(self.x + wa + gap, self.y, room - wa, self.h),
                )
            }
            Axis::Column => {
                let room = (self.h - gap).max(0.0);
                let ha = (room * ratio).round();
                (
                    Area::new(self.x, self.y, self.w, ha),
                    Area::new(self.x, self.y + ha + gap, self.w, room - ha),
                )
            }
        }
    }
}

/// A draggable divider: where it is, and which split it moves.
#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    /// Steps from the root to the split, `false` = into `a`.
    pub path: Vec<bool>,
    pub axis: Axis,
    /// The gap itself, for hit-testing and painting.
    pub area: Area,
    /// The space the split divides, to turn a pointer position into a ratio.
    pub parent: Area,
}

impl Node {
    /// The terminals, in reading order (left before right, top before bottom).
    pub fn leaves(&self) -> Vec<TabId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<TabId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    pub fn contains(&self, id: TabId) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Never: a tree holds at least one terminal.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Split { a, b, .. } => a.len() + b.len(),
        }
    }

    /// Put `new` beside `target`: after it (right / below) or before it.
    /// Returns whether `target` was found.
    pub fn split_leaf(&mut self, target: TabId, new: TabId, axis: Axis, before: bool) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let (a, b) = if before {
                    (Node::Leaf(new), Node::Leaf(target))
                } else {
                    (Node::Leaf(target), Node::Leaf(new))
                };
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    a: Box::new(a),
                    b: Box::new(b),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                a.split_leaf(target, new, axis, before) || b.split_leaf(target, new, axis, before)
            }
        }
    }

    /// Trade the places of two terminals.
    pub fn swap_leaves(&mut self, x: TabId, y: TabId) {
        match self {
            Node::Leaf(id) if *id == x => *id = y,
            Node::Leaf(id) if *id == y => *id = x,
            Node::Leaf(_) => {}
            Node::Split { a, b, .. } => {
                a.swap_leaves(x, y);
                b.swap_leaves(x, y);
            }
        }
    }

    /// Take `target` out; its sibling takes the space. `None` when the tree
    /// was that one leaf and nothing is left.
    pub fn remove(self, target: TabId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split { axis, ratio, a, b } => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// The terminal that would take `target`'s place if it went: the first
    /// leaf of its sibling. What focus moves to when it closes.
    pub fn heir(&self, target: TabId) -> Option<TabId> {
        match self {
            Node::Leaf(_) => None,
            Node::Split { a, b, .. } => {
                if **a == Node::Leaf(target) {
                    return b.leaves().first().copied();
                }
                if **b == Node::Leaf(target) {
                    return a.leaves().last().copied();
                }
                a.heir(target).or_else(|| b.heir(target))
            }
        }
    }

    /// Every terminal's area inside `area`, with `gap` between neighbours.
    pub fn layout(&self, area: Area, gap: f32) -> Vec<(TabId, Area)> {
        let mut out = Vec::new();
        self.lay(area, gap, &mut out);
        out
    }

    fn lay(&self, area: Area, gap: f32, out: &mut Vec<(TabId, Area)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split { axis, ratio, a, b } => {
                let (ra, rb) = area.split(*axis, *ratio, gap);
                a.lay(ra, gap, out);
                b.lay(rb, gap, out);
            }
        }
    }

    /// Every divider inside `area`.
    pub fn dividers(&self, area: Area, gap: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        self.walk_dividers(area, gap, &mut Vec::new(), &mut out);
        out
    }

    fn walk_dividers(&self, area: Area, gap: f32, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
        if let Node::Split { axis, ratio, a, b } = self {
            let (ra, rb) = area.split(*axis, *ratio, gap);
            let gap_area = match axis {
                Axis::Row => Area::new(ra.x + ra.w, area.y, gap, area.h),
                Axis::Column => Area::new(area.x, ra.y + ra.h, area.w, gap),
            };
            out.push(Divider {
                path: path.clone(),
                axis: *axis,
                area: gap_area,
                parent: area,
            });
            path.push(false);
            a.walk_dividers(ra, gap, path, out);
            path.pop();
            path.push(true);
            b.walk_dividers(rb, gap, path, out);
            path.pop();
        }
    }

    fn at_path(&mut self, path: &[bool]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((&step, rest)) => match self {
                Node::Leaf(_) => None,
                Node::Split { a, b, .. } => if step { b } else { a }.at_path(rest),
            },
        }
    }

    /// Move the divider at `path` to `ratio`, kept off the edges.
    pub fn set_ratio(&mut self, path: &[bool], to: f32) {
        if let Some(Node::Split { ratio, .. }) = self.at_path(path) {
            *ratio = to.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        }
    }

    /// Every split back to an even share.
    pub fn equalize(&mut self) {
        if let Node::Split { ratio, a, b, .. } = self {
            *ratio = 0.5;
            a.equalize();
            b.equalize();
        }
    }

    /// Grow or shrink `target` along the axis of `dir` by `step` (a share
    /// of its parent): the nearest enclosing split on that axis moves.
    pub fn nudge(&mut self, target: TabId, dir: Direction, step: f32) -> bool {
        let axis = match dir {
            Direction::Left | Direction::Right => Axis::Row,
            Direction::Up | Direction::Down => Axis::Column,
        };
        let grow = matches!(dir, Direction::Right | Direction::Down);
        self.nudge_in(target, axis, if grow { step } else { -step })
    }

    fn nudge_in(&mut self, target: TabId, want: Axis, delta: f32) -> bool {
        let Node::Split { axis, ratio, a, b } = self else {
            return false;
        };
        // Deepest split first: the divider closest to the terminal moves.
        if a.contains(target) {
            if a.nudge_in(target, want, delta) {
                return true;
            }
        } else if b.contains(target) {
            if b.nudge_in(target, want, delta) {
                return true;
            }
        } else {
            return false;
        }
        if *axis != want {
            return false;
        }
        *ratio = (*ratio + delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        true
    }
}

/// The terminal next to `from` in `dir`, given the areas `layout` produced:
/// the nearest one whose span overlaps `from`'s across the direction of
/// travel, so moving right from a tall terminal lands on the one level with
/// it rather than one diagonally off.
pub fn neighbour(areas: &[(TabId, Area)], from: TabId, dir: Direction) -> Option<TabId> {
    let (_, here) = areas.iter().find(|(id, _)| *id == from)?;
    let (cx, cy) = here.center();
    let eps = 0.5;
    areas
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let (beyond, overlap) = match dir {
                Direction::Right => (
                    r.x >= here.x + here.w - eps,
                    overlaps(r.y, r.h, here.y, here.h),
                ),
                Direction::Left => (
                    r.x + r.w <= here.x + eps,
                    overlaps(r.y, r.h, here.y, here.h),
                ),
                Direction::Down => (
                    r.y >= here.y + here.h - eps,
                    overlaps(r.x, r.w, here.x, here.w),
                ),
                Direction::Up => (
                    r.y + r.h <= here.y + eps,
                    overlaps(r.x, r.w, here.x, here.w),
                ),
            };
            if !beyond || overlap <= 0.0 {
                return None;
            }
            let (ox, oy) = r.center();
            let gap = match dir {
                Direction::Right => r.x - (here.x + here.w),
                Direction::Left => here.x - (r.x + r.w),
                Direction::Down => r.y - (here.y + here.h),
                Direction::Up => here.y - (r.y + r.h),
            };
            // Nearest edge first, then the one most level with us.
            let off = match dir {
                Direction::Left | Direction::Right => (oy - cy).abs(),
                Direction::Up | Direction::Down => (ox - cx).abs(),
            };
            Some((*id, gap, off))
        })
        .min_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        })
        .map(|(id, _, _)| id)
}

fn overlaps(a: f32, alen: f32, b: f32, blen: f32) -> f32 {
    (a + alen).min(b + blen) - a.max(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: TabId = TabId(1);
    const B: TabId = TabId(2);
    const C: TabId = TabId(3);

    /// a | (b / c)
    fn three() -> Node {
        let mut n = Node::Leaf(A);
        assert!(n.split_leaf(A, B, Axis::Row, false));
        assert!(n.split_leaf(B, C, Axis::Column, false));
        n
    }

    #[test]
    fn splitting_puts_the_new_terminal_beside_its_target() {
        let n = three();
        assert_eq!(n.leaves(), vec![A, B, C]);
        let mut m = Node::Leaf(A);
        m.split_leaf(A, B, Axis::Row, true);
        assert_eq!(m.leaves(), vec![B, A], "before puts it on the left");
        assert!(!m.clone().split_leaf(C, TabId(9), Axis::Row, false));
    }

    #[test]
    fn removing_a_leaf_lets_its_sibling_take_the_space() {
        let n = three().remove(B).unwrap();
        assert_eq!(n.leaves(), vec![A, C]);
        let Node::Split { axis, b, .. } = &n else {
            panic!()
        };
        assert_eq!(*axis, Axis::Row);
        assert_eq!(**b, Node::Leaf(C), "c moved up into b's split's place");

        let n = n.remove(A).unwrap();
        assert_eq!(n, Node::Leaf(C));
        assert_eq!(n.remove(C), None);
    }

    #[test]
    fn heir_is_the_sibling_nearest_the_closed_terminal() {
        let n = three();
        assert_eq!(n.heir(B), Some(C));
        assert_eq!(n.heir(C), Some(B));
        assert_eq!(
            n.heir(A),
            Some(B),
            "a's sibling is the stack; its first leaf"
        );
        assert_eq!(Node::Leaf(A).heir(A), None);
    }

    #[test]
    fn layout_fills_the_area_with_gaps_between() {
        let areas = three().layout(Area::new(0.0, 0.0, 201.0, 101.0), 1.0);
        assert_eq!(areas[0], (A, Area::new(0.0, 0.0, 100.0, 101.0)));
        assert_eq!(areas[1], (B, Area::new(101.0, 0.0, 100.0, 50.0)));
        assert_eq!(areas[2], (C, Area::new(101.0, 51.0, 100.0, 50.0)));
    }

    #[test]
    fn dividers_sit_in_the_gaps_and_move_their_own_split() {
        let mut n = three();
        let divs = n.dividers(Area::new(0.0, 0.0, 201.0, 101.0), 1.0);
        assert_eq!(divs.len(), 2);
        assert_eq!(divs[0].area, Area::new(100.0, 0.0, 1.0, 101.0));
        assert_eq!(divs[1].path, vec![true]);
        assert_eq!(divs[1].axis, Axis::Column);

        n.set_ratio(&divs[1].path, 0.95);
        let Node::Split { b, .. } = &n else { panic!() };
        let Node::Split { ratio, .. } = **b else {
            panic!()
        };
        assert_eq!(ratio, 1.0 - MIN_RATIO, "kept off the edge");
        n.equalize();
        assert_eq!(n, three());
    }

    #[test]
    fn nudge_moves_the_nearest_split_on_that_axis() {
        let mut n = three();
        assert!(n.nudge(C, Direction::Up, 0.1), "the stack's divider");
        let Node::Split {
            ratio: outer, b, ..
        } = &n
        else {
            panic!()
        };
        let Node::Split { ratio: inner, .. } = **b else {
            panic!()
        };
        assert_eq!(*outer, 0.5);
        assert!((inner - 0.4).abs() < 1e-6);

        assert!(
            n.nudge(C, Direction::Left, 0.1),
            "no row split in the stack: the outer one"
        );
        let Node::Split { ratio: outer, .. } = &n else {
            panic!()
        };
        assert!((outer - 0.4).abs() < 1e-6);
        assert!(!Node::Leaf(A).clone().nudge(A, Direction::Left, 0.1));
    }

    #[test]
    fn neighbours_follow_the_layout() {
        let areas = three().layout(Area::new(0.0, 0.0, 201.0, 101.0), 1.0);
        assert_eq!(
            neighbour(&areas, A, Direction::Right),
            Some(B),
            "level with a's top half wins the tie"
        );
        assert_eq!(neighbour(&areas, B, Direction::Down), Some(C));
        assert_eq!(neighbour(&areas, C, Direction::Up), Some(B));
        assert_eq!(neighbour(&areas, C, Direction::Left), Some(A));
        assert_eq!(neighbour(&areas, A, Direction::Left), None);
        assert_eq!(neighbour(&areas, B, Direction::Up), None);
    }

    #[test]
    fn swapping_trades_places_and_keeps_the_shape() {
        let mut n = three();
        n.swap_leaves(A, C);
        assert_eq!(n.leaves(), vec![C, B, A]);
    }

    #[test]
    fn serde_roundtrip() {
        let n = three();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), n);
    }
}
