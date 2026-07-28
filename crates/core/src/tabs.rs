//! The workspace model: categories and tabs, their order, and the active tab.
//!
//! Pure data + operations (no UI, no processes) so it unit-tests cleanly and
//! serializes for the persistence milestone. Runtime-only fields (exited,
//! git branch) are `serde(skip)`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CategoryId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: CategoryId,
    pub name: String,
    /// Index into the app's category palette (custom colors come with config).
    pub color_index: usize,
    pub collapsed: bool,
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
    #[serde(skip)]
    pub git_branch: Option<String>,
    #[serde(skip)]
    pub exited: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    next_id: u64,
    /// Rail order.
    pub categories: Vec<Category>,
    /// Rail order within each category (filter by `category`).
    pub tabs: Vec<Tab>,
    pub active: Option<TabId>,
}

impl Default for Workspace {
    fn default() -> Self {
        let mut ws = Workspace { next_id: 1, categories: Vec::new(), tabs: Vec::new(), active: None };
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
        });
        id
    }

    /// Remove a category; its tabs move to the first remaining category.
    /// The last category cannot be removed.
    pub fn remove_category(&mut self, id: CategoryId) -> bool {
        if self.categories.len() <= 1 {
            return false;
        }
        let Some(pos) = self.categories.iter().position(|c| c.id == id) else { return false };
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
                git_branch: None,
                exited: false,
            },
        );
        self.active = Some(id);
        id
    }

    /// Close a tab; picks a sensible new active tab (next in rail order,
    /// else previous, else none).
    pub fn close_tab(&mut self, id: TabId) {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else { return };
        self.tabs.remove(pos);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(pos)
                .or_else(|| self.tabs.get(pos.wrapping_sub(1)))
                .map(|t| t.id);
        }
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

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tab(id))
    }

    pub fn set_active(&mut self, id: TabId) {
        if self.tab(id).is_some() {
            self.active = Some(id);
        }
    }

    /// Move active selection by `delta` in rail order (wraps).
    pub fn cycle_active(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i32;
        let current = self
            .active
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len) as usize;
        self.active = Some(self.tabs[next].id);
    }

    pub fn move_tab_to_category(&mut self, id: TabId, category: CategoryId) {
        if self.category(category).is_none() {
            return;
        }
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else { return };
        let mut tab = self.tabs.remove(pos);
        tab.category = category;
        let insert_at = self
            .tabs
            .iter()
            .rposition(|t| t.category == category)
            .map(|i| i + 1)
            .unwrap_or(self.tabs.len());
        self.tabs.insert(insert_at, tab);
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
    fn add_and_close_fixes_active() {
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let a = ws.add_tab(cat);
        let b = ws.add_tab(cat);
        let c = ws.add_tab(cat);
        assert_eq!(ws.active, Some(c));

        ws.set_active(b);
        ws.close_tab(b);
        assert_eq!(ws.active, Some(c), "next in rail order");

        ws.close_tab(c);
        assert_eq!(ws.active, Some(a), "previous when no next");

        ws.close_tab(a);
        assert_eq!(ws.active, None);
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
}
