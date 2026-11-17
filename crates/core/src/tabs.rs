//! The workspace model: categories and tabs, their order, and the active tab.
//!
//! Pure data + operations (no UI, no processes) so it unit-tests cleanly and
//! serializes for the persistence milestone. Runtime-only fields (exited,
//! git branch) are `serde(skip)`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::panes::{Axis, Node};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

/// A rail row holding several terminals in a split layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub u64);

/// Several terminals sharing one rail row, laid out by `tree`. A row of one
/// terminal is not a group: it is a tab with `group: None`, which is what
/// every tab from before splits existed loads as.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: GroupId,
    pub tree: Node,
    /// The terminal last typed into here, which selecting the row returns to.
    pub focus: TabId,
    /// Showing only the focused terminal, full size, for now.
    #[serde(skip)]
    pub zoomed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CategoryId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: CategoryId,
    pub name: String,
    /// Index into the app's category palette (custom colors come with config).
    pub color_index: usize,
    pub collapsed: bool,
    /// Default Claude account for tabs in this category: a
    /// `CLAUDE_CONFIG_DIR` injected at spawn. `None` = inherit environment.
    #[serde(default)]
    pub profile_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub category: CategoryId,
    /// User-set title; wins over `auto_title` when present.
    pub custom_title: Option<String>,
    /// Live title from OSC 0/2 (or a default).
    #[serde(default)]
    pub auto_title: String,
    pub cwd: Option<PathBuf>,
    /// Live Claude Code session in this tab (captured via hooks) — the
    /// resume target after a restart.
    #[serde(default)]
    pub claude_session: Option<String>,
    /// Provider-tagged sessions; legacy Claude fields remain readable.
    #[serde(default)]
    pub agent_session: Option<crate::agent::AgentSession>,
    /// The `CLAUDE_CONFIG_DIR` that session ran under (multi-account).
    #[serde(default)]
    pub claude_config_dir: Option<PathBuf>,
    /// Command last seen running in this tab, so a restored tab can start it
    /// again (only when it is on the restore allowlist).
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(skip)]
    pub git_branch: Option<String>,
    /// The repository this tab is in, for the rail's by-repository view.
    /// Persisted rather than derived on load: a restored tab groups correctly
    /// before anything has looked at the filesystem for it.
    #[serde(default)]
    pub git_repo: Option<PathBuf>,
    /// The WSL distribution the tab opened in, when it did. What makes a unix
    /// `cwd` reachable from Windows — as `\\wsl.localhost\<distro>\…` — for
    /// the git lookups that would otherwise silently find nothing.
    #[serde(default)]
    pub wsl_distro: Option<String>,
    #[serde(skip)]
    pub exited: bool,
    /// The split row this terminal is part of, if any.
    #[serde(default)]
    pub group: Option<GroupId>,
}

impl Tab {
    pub fn title(&self) -> &str {
        if let Some(t) = &self.custom_title
            && !t.is_empty()
        {
            return t;
        }
        if !self.auto_title.is_empty() {
            return &self.auto_title;
        }
        "shell"
    }

    /// The title as shown in the rail: tidied per `[titles]`, but only when
    /// the shell chose it. A name you typed yourself is never rewritten.
    pub fn display_title(&self, titles: &crate::config::TitlesConfig) -> String {
        match &self.custom_title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => crate::config::display_title(self.title(), titles),
        }
    }
}

/// What one window is showing: its current tab and the ones before it.
/// The workspace holds the focus of the window being drawn; the others
/// park theirs here until their turn ([`Workspace::swap_focus`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Focus {
    pub active: Option<TabId>,
    #[serde(default)]
    pub recent: Vec<TabId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    next_id: u64,
    /// Rail order.
    pub categories: Vec<Category>,
    /// Rail order within each category (filter by `category`).
    pub tabs: Vec<Tab>,
    pub active: Option<TabId>,
    /// Tabs used before the active one, most recent first — what Ctrl+Tab
    /// walks. Kept here rather than in the app so that every way of changing
    /// tabs feeds it, and so it survives a restart with the tabs themselves.
    #[serde(default)]
    recent: Vec<TabId>,
    /// Split rows. Their members sit next to each other in `tabs`, in one
    /// category, so the rail draws each group once where its first member is.
    #[serde(default)]
    pub groups: Vec<Group>,
}

impl Default for Workspace {
    fn default() -> Self {
        let mut ws = Workspace {
            next_id: 1,
            categories: Vec::new(),
            tabs: Vec::new(),
            active: None,
            recent: Vec::new(),
            groups: Vec::new(),
        };
        ws.add_category("main");
        ws
    }
}

impl Workspace {
    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_category(&mut self, name: &str) -> CategoryId {
        let id = CategoryId(self.fresh_id());
        let color_index = self.categories.len();
        self.categories.push(Category {
            id,
            name: name.to_string(),
            color_index,
            collapsed: false,
            profile_dir: None,
        });
        id
    }

    /// Remove a category; its tabs move to the first remaining category.
    /// The last category cannot be removed.
    pub fn remove_category(&mut self, id: CategoryId) -> bool {
        if self.categories.len() <= 1 {
            return false;
        }
        let Some(pos) = self.categories.iter().position(|c| c.id == id) else {
            return false;
        };
        self.categories.remove(pos);
        let fallback = self.categories[0].id;
        for tab in &mut self.tabs {
            if tab.category == id {
                tab.category = fallback;
            }
        }
        true
    }

    pub fn category(&self, id: CategoryId) -> Option<&Category> {
        self.categories.iter().find(|c| c.id == id)
    }

    pub fn category_mut(&mut self, id: CategoryId) -> Option<&mut Category> {
        self.categories.iter_mut().find(|c| c.id == id)
    }

    /// Create a tab at the end of `category` and make it active.
    pub fn add_tab(&mut self, category: CategoryId) -> TabId {
        let id = TabId(self.fresh_id());
        let insert_at = self
            .tabs
            .iter()
            .rposition(|t| t.category == category)
            .map(|i| i + 1)
            .unwrap_or(self.tabs.len());
        self.tabs.insert(
            insert_at,
            Tab {
                id,
                category,
                custom_title: None,
                auto_title: String::new(),
                cwd: None,
                foreground: None,
                claude_session: None,
                agent_session: None,
                claude_config_dir: None,
                git_branch: None,
                git_repo: None,
                wsl_distro: None,
                exited: false,
                group: None,
            },
        );
        self.switch_to(id);
        // A new tab is the active one, and a collapsed category would hide it:
        // the rail would show no selection anywhere while you typed into it.
        // Opening the category is what asking for a tab in it means.
        if let Some(cat) = self.category_mut(category) {
            cat.collapsed = false;
        }
        id
    }

    /// Close a tab. Closing the active one goes back to the tab used before
    /// it — the one Ctrl+Tab would switch to — and only when there is none
    /// to the next in rail order, else the previous, else none.
    pub fn close_tab(&mut self, id: TabId) {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        // A terminal closing inside a split hands its space, and the
        // keyboard, to its neighbour in the layout rather than leaving the row.
        let heir = self.leave_group(id);
        self.tabs.remove(pos);
        if self.active == Some(id)
            && let Some(heir) = heir
        {
            self.active = Some(heir);
        } else if self.active == Some(id) {
            self.active = self
                .recent
                .iter()
                .copied()
                .find(|&x| self.tab(x).is_some())
                .or_else(|| {
                    self.tabs
                        .get(pos)
                        .or_else(|| self.tabs.get(pos.wrapping_sub(1)))
                        .map(|t| t.id)
                });
        }
        // A closed tab is nobody's "previous", and the tab that replaced it
        // is the current one rather than a step back from it.
        self.recent.retain(|&x| x != id && Some(x) != self.active);
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn tabs_in(&self, category: CategoryId) -> impl Iterator<Item = &Tab> {
        self.tabs.iter().filter(move |t| t.category == category)
    }

    /// Trade the focus being used for `other`'s: how a second window gets
    /// the same tab operations as the first, each with its own selection.
    pub fn swap_focus(&mut self, other: &mut Focus) {
        std::mem::swap(&mut self.active, &mut other.active);
        std::mem::swap(&mut self.recent, &mut other.recent);
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tab(id))
    }

    pub fn set_active(&mut self, id: TabId) {
        if self.tab(id).is_some() {
            self.switch_to(id);
        }
    }

    /// Make `id` active and push the tab it replaces onto the recency stack.
    fn switch_to(&mut self, id: TabId) {
        if self.active == Some(id) {
            return;
        }
        self.note_focus(id);
        // Moving between the terminals of one row is not switching rows:
        // Ctrl+Tab still goes back to the row you came from.
        if let Some(prev) = self.active
            && self.same_row(prev, id)
        {
            self.active = Some(id);
            return;
        }
        if let Some(prev) = self.active {
            self.recent.retain(|&x| x != prev);
            self.recent.insert(0, prev);
        }
        self.recent.retain(|&x| x != id);
        self.active = Some(id);
    }

    /// Select without disturbing the recency stack, for a Ctrl+Tab walk:
    /// stepping past a tab is not using it, so the order has to hold still
    /// until the walk ends. Pair with `commit_switch`.
    pub fn preview_active(&mut self, id: TabId) {
        if self.tab(id).is_some() {
            self.note_focus(id);
            self.active = Some(id);
        }
    }

    /// End a walk that began on `from`: whatever it landed on is now the
    /// current tab, and `from` is the one step back from it.
    pub fn commit_switch(&mut self, from: TabId) {
        let Some(landed) = self.active else { return };
        // A tab closed mid-walk is not a step back from anywhere.
        self.active = self.tab(from).map(|t| t.id);
        self.switch_to(landed);
    }

    /// Rows in most-recently-used order, the active one first, with rows
    /// never activated (a restored workspace, before anything is clicked)
    /// last in rail order. A split row appears once, as the terminal it
    /// would select.
    pub fn recent_order(&self) -> Vec<TabId> {
        let mut out: Vec<TabId> = Vec::with_capacity(self.tabs.len());
        let push = |ws: &Self, out: &mut Vec<TabId>, id: TabId| {
            if ws.tab(id).is_some() && !out.iter().any(|&x| ws.same_row(x, id)) {
                out.push(id);
            }
        };
        if let Some(active) = self.active {
            push(self, &mut out, active);
        }
        for &id in &self.recent {
            push(self, &mut out, self.row_target(id));
        }
        for tab in &self.tabs {
            push(self, &mut out, self.row_target(tab.id));
        }
        out
    }

    /// Move active selection by `delta` rows in rail order (wraps).
    pub fn cycle_active(&mut self, delta: i32) {
        let rows = self.row_heads();
        if rows.is_empty() {
            return;
        }
        let len = rows.len() as i32;
        let current = self
            .active
            .and_then(|id| rows.iter().position(|&r| self.same_row(r, id)))
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len) as usize;
        self.switch_to(self.row_target(rows[next]));
    }

    /// Move a category to `index` in rail order. Tabs follow their category:
    /// they are stored with it, so nothing about them changes.
    pub fn reorder_category(&mut self, id: CategoryId, index: usize) {
        let Some(from) = self.categories.iter().position(|c| c.id == id) else {
            return;
        };
        let to = index.min(self.categories.len().saturating_sub(1));
        if from == to {
            return;
        }
        let moved = self.categories.remove(from);
        self.categories.insert(to, moved);
    }

    /// Move `id`'s row into `category` at `index` counted among that
    /// category's rows (clamped). Used by rail drag-and-drop.
    pub fn reorder_tab(&mut self, id: TabId, category: CategoryId, index: usize) {
        if self.category(category).is_none() || self.tab(id).is_none() {
            return;
        }
        let block = self.take_row(id, category);
        // Translate the within-category row index into an absolute rail index.
        let heads: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| t.category == category && self.is_head_at(*i))
            .map(|(i, _)| i)
            .collect();
        let at = match heads.get(index) {
            Some(&i) => i,
            None => self.end_of(category),
        };
        self.tabs.splice(at..at, block);
    }

    pub fn move_tab_to_category(&mut self, id: TabId, category: CategoryId) {
        if self.category(category).is_none() || self.tab(id).is_none() {
            return;
        }
        let block = self.take_row(id, category);
        let at = self.end_of(category);
        self.tabs.splice(at..at, block);
    }

    /// Lift `id`'s whole row out of `tabs`, retagged to `category`.
    fn take_row(&mut self, id: TabId, category: CategoryId) -> Vec<Tab> {
        let group = self.tab(id).and_then(|t| t.group);
        let mut block = Vec::new();
        let mut i = 0;
        while i < self.tabs.len() {
            let t = &self.tabs[i];
            if t.id == id || (group.is_some() && t.group == group) {
                let mut t = self.tabs.remove(i);
                t.category = category;
                block.push(t);
            } else {
                i += 1;
            }
        }
        block
    }

    /// Index just past `category`'s last tab, or the end.
    fn end_of(&self, category: CategoryId) -> usize {
        self.tabs
            .iter()
            .rposition(|t| t.category == category)
            .map(|i| i + 1)
            .unwrap_or(self.tabs.len())
    }

    // ── split rows ──────────────────────────────────────────────────────

    pub fn group(&self, id: GroupId) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: GroupId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// The split row `tab` is in, if it is in one.
    pub fn group_of(&self, tab: TabId) -> Option<&Group> {
        self.tab(tab)
            .and_then(|t| t.group)
            .and_then(|g| self.group(g))
    }

    pub fn group_of_mut(&mut self, tab: TabId) -> Option<&mut Group> {
        let g = self.tab(tab).and_then(|t| t.group)?;
        self.group_mut(g)
    }

    /// The terminals on screen when `tab` is: its row's, in layout order.
    pub fn row_tabs(&self, tab: TabId) -> Vec<TabId> {
        match self.group_of(tab) {
            Some(g) => g.tree.leaves(),
            None if self.tab(tab).is_some() => vec![tab],
            None => Vec::new(),
        }
    }

    /// The terminals on screen now. A zoomed row shows only its focus.
    pub fn visible(&self) -> Vec<TabId> {
        let Some(active) = self.active else {
            return Vec::new();
        };
        match self.group_of(active) {
            Some(g) if g.zoomed => vec![active],
            _ => self.row_tabs(active),
        }
    }

    pub fn same_row(&self, a: TabId, b: TabId) -> bool {
        if a == b {
            return true;
        }
        let ga = self.tab(a).and_then(|t| t.group);
        ga.is_some() && ga == self.tab(b).and_then(|t| t.group)
    }

    /// The terminal selecting `tab`'s row lands on: the one last focused.
    pub fn row_target(&self, tab: TabId) -> TabId {
        self.group_of(tab).map(|g| g.focus).unwrap_or(tab)
    }

    /// Whether the rail draws `tab`'s row at `tab`: it is the row's first
    /// member in rail order.
    pub fn is_row_head(&self, tab: TabId) -> bool {
        self.tabs
            .iter()
            .position(|t| t.id == tab)
            .is_some_and(|i| self.is_head_at(i))
    }

    fn is_head_at(&self, i: usize) -> bool {
        match self.tabs[i].group {
            None => true,
            Some(g) => i == 0 || self.tabs[i - 1].group != Some(g),
        }
    }

    /// The first member of every row, in rail order.
    pub fn row_heads(&self) -> Vec<TabId> {
        (0..self.tabs.len())
            .filter(|&i| self.is_head_at(i))
            .map(|i| self.tabs[i].id)
            .collect()
    }

    fn note_focus(&mut self, id: TabId) {
        if let Some(g) = self.group_of_mut(id) {
            if g.focus != id {
                g.zoomed = false;
            }
            g.focus = id;
        }
    }

    /// Open a new terminal beside `from`, in its row (making the row a
    /// split if it was one terminal), and focus it. `axis` is the split's
    /// direction; `before` puts the new one left of or above `from`.
    pub fn split(&mut self, from: TabId, axis: Axis, before: bool) -> Option<TabId> {
        let category = self.tab(from)?.category;
        let id = TabId(self.fresh_id());
        let group = match self.tab(from).and_then(|t| t.group) {
            Some(g) => g,
            None => {
                let g = GroupId(self.fresh_id());
                self.groups.push(Group {
                    id: g,
                    tree: Node::Leaf(from),
                    focus: from,
                    zoomed: false,
                });
                self.tab_mut(from)?.group = Some(g);
                g
            }
        };
        let grp = self.group_mut(group)?;
        grp.zoomed = false;
        grp.tree.split_leaf(from, id, axis, before);
        let at = self.row_end(from);
        self.tabs.insert(
            at,
            Tab {
                id,
                category,
                custom_title: None,
                auto_title: String::new(),
                cwd: None,
                foreground: None,
                claude_session: None,
                agent_session: None,
                claude_config_dir: None,
                git_branch: None,
                git_repo: None,
                wsl_distro: None,
                exited: false,
                group: Some(group),
            },
        );
        self.switch_to(id);
        Some(id)
    }

    /// Index just past the last member of `tab`'s row.
    fn row_end(&self, tab: TabId) -> usize {
        let group = self.tab(tab).and_then(|t| t.group);
        self.tabs
            .iter()
            .rposition(|t| t.id == tab || (group.is_some() && t.group == group))
            .map(|i| i + 1)
            .unwrap_or(self.tabs.len())
    }

    /// Take `id` out of its split, if it is in one. The group dissolves
    /// when one terminal is left. Returns who takes over its space.
    fn leave_group(&mut self, id: TabId) -> Option<TabId> {
        let gid = self.tab(id)?.group?;
        if let Some(t) = self.tab_mut(id) {
            t.group = None;
        }
        let idx = self.groups.iter().position(|g| g.id == gid)?;
        let heir = self.groups[idx].tree.heir(id);
        let tree = std::mem::replace(&mut self.groups[idx].tree, Node::Leaf(id));
        match tree.remove(id) {
            Some(rest) if rest.len() > 1 => {
                let g = &mut self.groups[idx];
                if g.focus == id {
                    g.focus = heir.unwrap_or_else(|| rest.leaves()[0]);
                }
                g.tree = rest;
                g.zoomed = false;
            }
            rest => {
                self.groups.remove(idx);
                for leaf in rest.map(|n| n.leaves()).unwrap_or_default() {
                    if let Some(t) = self.tab_mut(leaf) {
                        t.group = None;
                    }
                }
            }
        }
        heir
    }

    /// Move `id` out of its split onto a row of its own, just after the row
    /// it left. Focus stays with it.
    pub fn detach(&mut self, id: TabId) {
        if self.tab(id).and_then(|t| t.group).is_none() {
            return;
        }
        let anchor = self.row_tabs(id).into_iter().find(|&t| t != id);
        self.leave_group(id);
        if let Some(anchor) = anchor {
            let pos = self.tabs.iter().position(|t| t.id == id).unwrap();
            let tab = self.tabs.remove(pos);
            let at = self.row_end(anchor);
            self.tabs.insert(at, tab);
        }
        // It was in the active row; now it is a row, and the one you're in.
        if self.active == Some(id) {
            self.recent.retain(|&x| x != id);
        }
    }

    /// Trade the places of two terminals of one split.
    pub fn swap_in_group(&mut self, a: TabId, b: TabId) {
        if a == b || !self.same_row(a, b) {
            return;
        }
        if let Some(g) = self.group_of_mut(a) {
            g.tree.swap_leaves(a, b);
            g.zoomed = false;
        }
    }

    /// Take `id` out of its split onto a row of its own at `index` among
    /// `category`'s rows: a terminal dragged out of a split in the rail.
    pub fn detach_to(&mut self, id: TabId, category: CategoryId, index: usize) {
        self.detach(id);
        self.reorder_tab(id, category, index);
    }

    /// Put `id` (with its row, if it had one, broken up) into `onto`'s
    /// row, beside `onto`. The rail drop that builds a split.
    pub fn join(&mut self, id: TabId, onto: TabId, axis: Axis, before: bool) {
        if id == onto || self.tab(id).is_none() || self.tab(onto).is_none() {
            return;
        }
        self.leave_group(id);
        let category = self.tab(onto).map(|t| t.category).unwrap();
        let group = match self.tab(onto).and_then(|t| t.group) {
            Some(g) => g,
            None => {
                let g = GroupId(self.fresh_id());
                self.groups.push(Group {
                    id: g,
                    tree: Node::Leaf(onto),
                    focus: onto,
                    zoomed: false,
                });
                self.tab_mut(onto).unwrap().group = Some(g);
                g
            }
        };
        let grp = self.group_mut(group).unwrap();
        grp.tree.split_leaf(onto, id, axis, before);
        grp.zoomed = false;
        let pos = self.tabs.iter().position(|t| t.id == id).unwrap();
        let mut tab = self.tabs.remove(pos);
        tab.category = category;
        tab.group = Some(group);
        let at = self.row_end(onto);
        self.tabs.insert(at, tab);
        self.recent.retain(|&x| x != id);
        self.switch_to(id);
    }

    /// Make a loaded workspace consistent: every group's tree names exactly
    /// its live members, a group of fewer than two dissolves, and members
    /// sit together in one category. A hand-edited or half-written file
    /// costs a split, never a terminal.
    pub fn normalize(&mut self) {
        let live: Vec<TabId> = self.tabs.iter().map(|t| t.id).collect();
        let mut groups = std::mem::take(&mut self.groups);
        groups.retain_mut(|g| {
            let mut tree = Some(g.tree.clone());
            for leaf in g.tree.leaves() {
                let member = self.tab(leaf).is_some_and(|t| t.group == Some(g.id));
                if !live.contains(&leaf) || !member {
                    tree = tree.and_then(|n| n.remove(leaf));
                }
            }
            match tree {
                Some(t) if t.len() > 1 => {
                    if !t.contains(g.focus) {
                        g.focus = t.leaves()[0];
                    }
                    g.tree = t;
                    true
                }
                _ => false,
            }
        });
        for t in &mut self.tabs {
            if let Some(g) = t.group
                && !groups.iter().any(|x| x.id == g && x.tree.contains(t.id))
            {
                t.group = None;
            }
        }
        self.groups = groups;
        // Pull each group's members together behind its first one.
        for g in self.groups.clone() {
            let leaves = g.tree.leaves();
            let Some(first) = self.tabs.iter().position(|t| t.group == Some(g.id)) else {
                continue;
            };
            let category = self.tabs[first].category;
            let mut block: Vec<Tab> = Vec::new();
            self.tabs.retain(|t| {
                if t.group == Some(g.id) {
                    block.push(t.clone());
                    false
                } else {
                    true
                }
            });
            block.sort_by_key(|t| leaves.iter().position(|&l| l == t.id));
            for t in &mut block {
                t.category = category;
            }
            let at = first.min(self.tabs.len());
            self.tabs.splice(at..at, block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_main_category() {
        let ws = Workspace::default();
        assert_eq!(ws.categories.len(), 1);
        assert_eq!(ws.categories[0].name, "main");
    }

    #[test]
    fn a_new_tab_opens_the_category_it_lands_in() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        ws.categories[0].collapsed = true;

        let id = ws.add_tab(cat);
        assert_eq!(ws.active, Some(id));
        assert!(
            !ws.categories[0].collapsed,
            "the new tab is the active one — a collapsed category would hide it"
        );
    }

    #[test]
    fn add_and_close_fixes_active() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        assert_eq!(ws.active, Some(c));

        ws.set_active(b);
        ws.close_tab(b);
        assert_eq!(ws.active, Some(c), "back to the tab used before it");

        ws.close_tab(c);
        assert_eq!(ws.active, Some(a), "back to the tab used before it");

        ws.close_tab(a);
        assert_eq!(ws.active, None);
    }

    /// Closing the current tab goes back to where you were before it, not
    /// to its neighbour in the rail (giverny#247).
    #[test]
    fn closing_the_active_tab_goes_back_to_the_last_one_used() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        ws.set_active(a);
        ws.set_active(c);
        ws.close_tab(c);
        assert_eq!(ws.active, Some(a), "a was used last, not b next to it");
        assert_eq!(ws.recent_order(), vec![a, b], "a left the stack");

        // Again from the middle of the rail: the neighbours lose to recency.
        let d = ws.add_tab(cat);
        ws.set_active(b);
        ws.set_active(d);
        ws.set_active(a);
        // Rail: a b d; used: a, d, b.
        ws.close_tab(a);
        assert_eq!(ws.active, Some(d));
        ws.close_tab(d);
        assert_eq!(ws.active, Some(b));
        assert_eq!(ws.recent_order(), vec![b]);
    }

    /// Tabs nobody has used since a restore have no recency; rail order is
    /// the fallback, next first and then previous.
    #[test]
    fn closing_with_no_recency_falls_back_to_rail_order() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        ws.recent.clear();
        ws.active = Some(b);
        ws.close_tab(b);
        assert_eq!(ws.active, Some(c), "next in rail order");

        ws.recent.clear();
        ws.close_tab(c);
        assert_eq!(ws.active, Some(a), "previous when no next");
    }

    /// Closing a tab that is not the active one leaves the selection alone
    /// and only drops it from the stack.
    #[test]
    fn closing_another_tab_keeps_the_active_one() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        ws.close_tab(b);
        assert_eq!(ws.active, Some(c));
        assert!(!ws.recent.contains(&b), "b left the stack");
        assert_eq!(ws.recent_order(), vec![c, a]);
        ws.close_tab(c);
        assert_eq!(ws.active, Some(a));
        assert_eq!(ws.recent_order(), vec![a]);
    }

    /// The Ctrl+Tab order: where you were, not where the tab sits in the rail.
    #[test]
    fn recency_order_follows_use() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        // Opening three tabs already ranks them: c is current, then b, then a.
        assert_eq!(ws.recent_order(), vec![c, b, a]);

        ws.set_active(a);
        assert_eq!(ws.recent_order(), vec![a, c, b]);
        ws.set_active(c);
        assert_eq!(ws.recent_order(), vec![c, a, b], "one press goes back to a");
    }

    /// A walk shows each tab it passes without counting it as used: only
    /// where it lands moves to the front, so pressing twice again goes two
    /// back rather than bouncing between the last two.
    #[test]
    fn a_walk_only_records_where_it_lands() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        let d = ws.add_tab(cat);
        assert_eq!(ws.recent_order(), vec![d, c, b, a]);

        let from = ws.active.unwrap();
        let order = ws.recent_order();
        ws.preview_active(order[1]);
        ws.preview_active(order[2]);
        assert_eq!(ws.active, Some(b), "stepped two back");
        assert_eq!(ws.recent_order()[1], c, "the order held still mid-walk");
        ws.commit_switch(from);
        assert_eq!(ws.recent_order(), vec![b, d, c, a]);
    }

    /// A tab closed during a walk is not somewhere to go back to.
    #[test]
    fn closing_clears_the_recency_stack() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        ws.close_tab(a);
        assert_eq!(ws.recent_order(), vec![b]);

        let c = ws.add_tab(cat);
        ws.preview_active(b);
        ws.close_tab(c);
        ws.commit_switch(c);
        assert_eq!(ws.recent_order(), vec![b]);
    }

    #[test]
    fn categories_can_be_reordered() {
        let mut ws = Workspace::default();
        let main = ws.categories[0].id;
        let work = ws.add_category("work");
        let infra = ws.add_category("infra");
        let tab = ws.add_tab(work);

        ws.reorder_category(infra, 0);
        let order: Vec<CategoryId> = ws.categories.iter().map(|c| c.id).collect();
        assert_eq!(order, vec![infra, main, work]);
        // A tab is stored with its category and does not notice.
        assert_eq!(ws.tab(tab).unwrap().category, work);

        // Past the end lands at the end; a move to where it already is is
        // not a move.
        ws.reorder_category(infra, 99);
        assert_eq!(ws.categories.last().unwrap().id, infra);
        ws.reorder_category(infra, 99);
        assert_eq!(ws.categories.last().unwrap().id, infra);
    }

    #[test]
    fn tabs_group_per_category() {
        let mut ws = Workspace::default();
        let main = ws.categories[0].id;
        let work = ws.add_category("work");
        let m1 = ws.add_tab(main);
        let w1 = ws.add_tab(work);
        let m2 = ws.add_tab(main);
        // m2 inserts after m1, before work's tabs in rail order.
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![m1, m2, w1]);
        assert_eq!(ws.tabs_in(main).count(), 2);
        assert_eq!(ws.tabs_in(work).count(), 1);
    }

    #[test]
    fn cycle_wraps() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        ws.set_active(b);
        ws.cycle_active(1);
        assert_eq!(ws.active, Some(a), "wraps forward");
        ws.cycle_active(-1);
        assert_eq!(ws.active, Some(b), "wraps backward");
    }

    #[test]
    fn title_precedence() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let id = ws.add_tab(cat);
        assert_eq!(ws.tab(id).unwrap().title(), "shell");
        ws.tab_mut(id).unwrap().auto_title = "zsh: ~/dev".into();
        assert_eq!(ws.tab(id).unwrap().title(), "zsh: ~/dev");
        ws.tab_mut(id).unwrap().custom_title = Some("api server".into());
        assert_eq!(ws.tab(id).unwrap().title(), "api server");
    }

    #[test]
    fn a_title_you_typed_is_never_tidied() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let id = ws.add_tab(cat);
        let titles = crate::config::TitlesConfig::default();

        ws.tab_mut(id).unwrap().auto_title = "yoz@box:~/Dev/bobo".into();
        assert_eq!(ws.tab(id).unwrap().display_title(&titles), "~/Dev/bobo");

        // Same shape, but chosen by the user: left exactly as typed.
        ws.tab_mut(id).unwrap().custom_title = Some("yoz@box:~/Dev/bobo".into());
        assert_eq!(
            ws.tab(id).unwrap().display_title(&titles),
            "yoz@box:~/Dev/bobo"
        );

        // Nor is a Claude Code glyph you typed into a name.
        let titles = crate::config::TitlesConfig {
            strip_claude_glyph: true,
            ..titles
        };
        ws.tab_mut(id).unwrap().auto_title = "✳ Fix the parser".into();
        ws.tab_mut(id).unwrap().custom_title = None;
        assert_eq!(ws.tab(id).unwrap().display_title(&titles), "Fix the parser");
        ws.tab_mut(id).unwrap().custom_title = Some("✳ mine".into());
        assert_eq!(ws.tab(id).unwrap().display_title(&titles), "✳ mine");
    }

    #[test]
    fn remove_category_moves_tabs() {
        let mut ws = Workspace::default();
        let main = ws.categories[0].id;
        let work = ws.add_category("work");
        let t = ws.add_tab(work);
        assert!(ws.remove_category(work));
        assert_eq!(ws.tab(t).unwrap().category, main);
        assert!(!ws.remove_category(main), "last category stays");
    }

    #[test]
    fn reorder_within_and_across_categories() {
        let mut ws = Workspace::default();
        let main = ws.categories[0].id;
        let work = ws.add_category("work");
        let a = ws.add_tab(main);
        let b = ws.add_tab(main);
        let c = ws.add_tab(main);
        let ids = |ws: &Workspace, cat| ws.tabs_in(cat).map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(&ws, main), vec![a, b, c]);

        // Drag the last tab to the front of its own category.
        ws.reorder_tab(c, main, 0);
        assert_eq!(ids(&ws, main), vec![c, a, b]);

        // Drop into an empty category.
        ws.reorder_tab(a, work, 0);
        assert_eq!(ids(&ws, work), vec![a]);
        assert_eq!(ids(&ws, main), vec![c, b]);

        // An out-of-range index appends rather than panicking.
        ws.reorder_tab(b, work, 99);
        assert_eq!(ids(&ws, work), vec![a, b]);
        assert_eq!(ids(&ws, main), vec![c]);
    }

    #[test]
    fn serde_roundtrip() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let id = ws.add_tab(cat);
        ws.tab_mut(id).unwrap().custom_title = Some("x".into());
        let json = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tab(id).unwrap().title(), "x");
        assert_eq!(back.active, Some(id));
    }

    // ── split rows ──────────────────────────────────────────────────────

    #[test]
    fn splitting_makes_a_row_of_two_and_focuses_the_new_terminal() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let other = ws.add_tab(cat);
        ws.set_active(a);
        let b = ws.split(a, Axis::Row, false).unwrap();
        assert_eq!(ws.active, Some(b));
        assert_eq!(ws.row_tabs(a), vec![a, b]);
        assert_eq!(ws.visible(), vec![a, b]);
        assert!(ws.same_row(a, b));
        // b sits right after a in the rail, before the other row.
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![a, b, other]);
        assert_eq!(ws.row_heads(), vec![a, other]);
        assert!(!ws.is_row_head(b));
    }

    #[test]
    fn moving_inside_a_row_is_not_switching_rows() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let x = ws.add_tab(cat);
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        ws.set_active(a);
        ws.set_active(b);
        assert_eq!(
            ws.recent_order(),
            vec![b, x],
            "the row once, then the row before it"
        );
        ws.set_active(x);
        assert_eq!(
            ws.recent_order(),
            vec![x, b],
            "the row comes back at its last focus"
        );
        assert_eq!(ws.row_target(a), b);
    }

    #[test]
    fn cycling_steps_over_rows_not_terminals() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let c = ws.add_tab(cat);
        ws.set_active(c);
        ws.cycle_active(1);
        assert_eq!(ws.active, Some(b), "wraps to the row, at its focus");
        ws.cycle_active(1);
        assert_eq!(ws.active, Some(c));
    }

    #[test]
    fn closing_a_split_terminal_hands_focus_to_its_neighbour() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let x = ws.add_tab(cat);
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let c = ws.split(b, Axis::Column, false).unwrap();
        ws.close_tab(c);
        assert_eq!(
            ws.active,
            Some(b),
            "the terminal beside it, not the previous row"
        );
        assert_eq!(ws.row_tabs(a), vec![a, b]);
        ws.close_tab(b);
        assert_eq!(ws.active, Some(a));
        assert!(ws.groups.is_empty(), "a row of one is no longer a group");
        assert_eq!(ws.tab(a).unwrap().group, None);
        ws.close_tab(a);
        assert_eq!(ws.active, Some(x));
    }

    #[test]
    fn a_row_moves_as_one() {
        let mut ws = Workspace::default();
        let main = ws.categories[0].id;
        let work = ws.add_category("work");
        let a = ws.add_tab(main);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let c = ws.add_tab(main);
        let ids = |ws: &Workspace, cat| ws.tabs_in(cat).map(|t| t.id).collect::<Vec<_>>();

        ws.reorder_tab(c, main, 0);
        assert_eq!(ids(&ws, main), vec![c, a, b]);
        ws.reorder_tab(b, main, 0);
        assert_eq!(
            ids(&ws, main),
            vec![a, b, c],
            "dragging any member drags the row"
        );
        ws.reorder_tab(c, main, 1);
        assert_eq!(
            ids(&ws, main),
            vec![a, b, c],
            "index 1 is after the row, not inside it"
        );
        ws.move_tab_to_category(a, work);
        assert_eq!(ids(&ws, work), vec![a, b]);
        assert_eq!(ids(&ws, main), vec![c]);
    }

    #[test]
    fn detach_and_join() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let c = ws.split(b, Axis::Column, false).unwrap();
        let x = ws.add_tab(cat);

        ws.set_active(c);
        ws.detach(c);
        assert_eq!(ws.row_tabs(a), vec![a, b]);
        assert_eq!(ws.row_tabs(c), vec![c]);
        assert_eq!(ws.active, Some(c));
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![a, b, c, x], "right after the row it left");

        ws.join(x, a, Axis::Column, true);
        assert_eq!(ws.row_tabs(a), vec![x, a, b]);
        assert_eq!(ws.active, Some(x));
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![a, b, x, c]);

        // Joining a terminal of one split onto another moves just it.
        ws.join(b, c, Axis::Row, false);
        assert_eq!(ws.row_tabs(c), vec![c, b]);
        assert_eq!(ws.row_tabs(a), vec![x, a]);
    }

    #[test]
    fn a_terminal_dragged_out_lands_where_it_was_dropped() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let x = ws.add_tab(cat);
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        // Rows: x, [a b]. Drop b above x.
        ws.detach_to(b, cat, 0);
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![b, x, a]);
        assert!(ws.groups.is_empty());

        let c = ws.split(a, Axis::Row, false).unwrap();
        ws.swap_in_group(a, c);
        assert_eq!(ws.row_tabs(a), vec![c, a]);
        ws.swap_in_group(a, x);
        assert_eq!(ws.row_tabs(a), vec![c, a], "not in one split: nothing");
    }

    #[test]
    fn zoom_shows_only_the_focus_until_focus_moves() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        ws.group_of_mut(b).unwrap().zoomed = true;
        assert_eq!(ws.visible(), vec![b]);
        ws.set_active(a);
        assert_eq!(ws.visible(), vec![a, b], "moving focus unzooms");
    }

    #[test]
    fn normalize_repairs_what_a_bad_file_can_hold() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let other = ws.add_category("other");
        let a = ws.add_tab(cat);
        let b = ws.split(a, Axis::Row, false).unwrap();
        let c = ws.split(b, Axis::Row, false).unwrap();
        let x = ws.add_tab(cat);
        // Scatter the row and lose a member, as a hand edit might.
        let pos = ws.tabs.iter().position(|t| t.id == c).unwrap();
        let mut moved = ws.tabs.remove(pos);
        moved.category = other;
        ws.tabs.push(moved);
        ws.tabs.retain(|t| t.id != b);
        ws.normalize();
        assert_eq!(ws.row_tabs(a), vec![a, c]);
        let order: Vec<TabId> = ws.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![a, c, x]);
        assert_eq!(ws.tab(c).unwrap().category, cat);

        ws.tabs.retain(|t| t.id != c);
        ws.normalize();
        assert!(ws.groups.is_empty());
        assert_eq!(ws.tab(a).unwrap().group, None);
    }

    #[test]
    fn swapping_focus_trades_selection_and_history() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let mut other = Focus {
            active: Some(a),
            recent: vec![],
        };
        ws.swap_focus(&mut other);
        assert_eq!(ws.active, Some(a));
        assert_eq!(other.active, Some(b));
        assert_eq!(
            other.recent,
            vec![a],
            "the first window's history went with it"
        );
        ws.swap_focus(&mut other);
        assert_eq!(ws.active, Some(b));
    }

    #[test]
    fn a_file_from_before_splits_loads_as_rows_of_one() {
        let json = r#"{"next_id":3,"categories":[{"id":1,"name":"main","color_index":0,"collapsed":false}],
            "tabs":[{"id":2,"category":1,"custom_title":null,"cwd":null}],"active":2}"#;
        let ws: Workspace = serde_json::from_str(json).unwrap();
        assert!(ws.groups.is_empty());
        assert_eq!(ws.visible(), vec![TabId(2)]);
    }
}
