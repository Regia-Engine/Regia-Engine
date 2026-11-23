// SPDX-License-Identifier: MPL-2.0
//! Tree view and data grid models (ADR-273 W10, ADR-1842).
//!
//! [`UiTree`] holds a tree of labels with an expanded set, a selection and keyboard navigation.
//! Its flattened visible rows ([`UiTree::rows`]) are what a virtualised list shows, so a tree of
//! 10 000 nodes costs one list row per visible entry, not one entity per node.
//! [`sync_tree_lists`] writes those rows into the [`ListItems`] of the entity that carries the tree
//! (next to a [`crate::scroll::ListView`]). [`DataGrid`] is a sortable table model with the same
//! flattening idea: columns, rows and a stable sort by one column.
//!
//! The agent surface is `ui.tree` (`book/agents/ui-tree.md`).

use std::collections::{BTreeSet, HashMap};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::scroll::{ListItem, ListItems};

/// Most nodes one tree may hold.
pub const MAX_NODES: usize = 200_000;
/// Deepest nesting accepted when building a tree.
pub const MAX_DEPTH: usize = 128;

/// A nested description of a tree, as the agent and files give it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeSpec {
    /// The node's label.
    pub label: String,
    /// Child nodes.
    #[serde(default)]
    pub children: Vec<TreeSpec>,
}

/// One visible row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRow {
    /// Node id (preorder index).
    pub node: usize,
    /// Nesting depth, 0 for roots.
    pub depth: usize,
    /// Whether the node has children.
    pub has_children: bool,
    /// Whether the node is expanded.
    pub expanded: bool,
}

/// Where a drop lands relative to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPosition {
    /// Above the row, as its previous sibling.
    Before,
    /// Onto the row, as its last child.
    Inside,
    /// Below the row, as its next sibling.
    After,
}

/// Why a tree could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// More than [`MAX_NODES`] nodes.
    #[error("a tree may hold at most {MAX_NODES} nodes")]
    TooManyNodes,
    /// Nested deeper than [`MAX_DEPTH`].
    #[error("a tree may nest at most {MAX_DEPTH} levels")]
    TooDeep,
}

/// A keyboard move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKey {
    /// Previous visible row.
    Up,
    /// Next visible row.
    Down,
    /// Collapse, or go to the parent.
    Left,
    /// Expand, or go to the first child.
    Right,
    /// First visible row.
    Home,
    /// Last visible row.
    End,
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    label: String,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// The tree model. See the module docs.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiTree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    expanded: BTreeSet<usize>,
    /// The selected node, if any.
    pub selected: Option<usize>,
    rows: Vec<TreeRow>,
    /// Bumped on every change; [`sync_tree_lists`] compares it.
    pub generation: u64,
    synced: u64,
}

impl UiTree {
    /// Builds a tree from `roots`, everything collapsed.
    pub fn new(roots: &[TreeSpec]) -> Result<Self, TreeError> {
        let mut tree = Self::default();
        // Iterative preorder walk so deep input cannot overflow the stack.
        let mut stack: Vec<(&TreeSpec, Option<usize>, usize)> =
            roots.iter().rev().map(|s| (s, None, 0)).collect();
        while let Some((spec, parent, depth)) = stack.pop() {
            if depth >= MAX_DEPTH {
                return Err(TreeError::TooDeep);
            }
            if tree.nodes.len() >= MAX_NODES {
                return Err(TreeError::TooManyNodes);
            }
            let id = tree.nodes.len();
            tree.nodes.push(Node {
                label: spec.label.clone(),
                parent,
                children: Vec::new(),
            });
            match parent {
                Some(p) => tree.nodes[p].children.push(id),
                None => tree.roots.push(id),
            }
            for child in spec.children.iter().rev() {
                stack.push((child, Some(id), depth + 1));
            }
        }
        tree.rebuild();
        Ok(tree)
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The label of `node`.
    pub fn label(&self, node: usize) -> Option<&str> {
        self.nodes.get(node).map(|n| n.label.as_str())
    }

    /// The parent of `node`.
    pub fn parent(&self, node: usize) -> Option<usize> {
        self.nodes.get(node).and_then(|n| n.parent)
    }

    /// The visible rows, top to bottom.
    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    /// Whether `node` is expanded.
    pub fn is_expanded(&self, node: usize) -> bool {
        self.expanded.contains(&node)
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        let mut stack: Vec<(usize, usize)> = self.roots.iter().rev().map(|r| (*r, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            let n = &self.nodes[id];
            let expanded = !n.children.is_empty() && self.expanded.contains(&id);
            self.rows.push(TreeRow {
                node: id,
                depth,
                has_children: !n.children.is_empty(),
                expanded,
            });
            if expanded {
                for c in n.children.iter().rev() {
                    stack.push((*c, depth + 1));
                }
            }
        }
        self.generation += 1;
    }

    /// Expands `node` (a leaf stays as it is). Returns whether anything changed.
    pub fn expand(&mut self, node: usize) -> bool {
        let changed =
            self.nodes.get(node).is_some_and(|n| !n.children.is_empty()) && self.expanded.insert(node);
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Collapses `node`. If the selection was inside it, the selection moves to `node`.
    pub fn collapse(&mut self, node: usize) -> bool {
        let changed = self.expanded.remove(&node);
        if changed {
            if let Some(sel) = self.selected
                && sel != node
                && self.is_descendant(sel, node)
            {
                self.selected = Some(node);
            }
            self.rebuild();
        }
        changed
    }

    /// Toggles `node`.
    pub fn toggle(&mut self, node: usize) -> bool {
        if self.expanded.contains(&node) {
            self.collapse(node)
        } else {
            self.expand(node)
        }
    }

    /// Expands every node that has children.
    pub fn expand_all(&mut self) {
        self.expanded = (0..self.nodes.len())
            .filter(|i| !self.nodes[*i].children.is_empty())
            .collect();
        self.rebuild();
    }

    /// Collapses everything; a selected node that gets hidden moves to its top-level ancestor.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        if let Some(sel) = self.selected {
            self.selected = Some(self.root_of(sel));
        }
        self.rebuild();
    }

    /// Expands every ancestor of `node` so its row is visible.
    pub fn reveal(&mut self, node: usize) -> bool {
        if node >= self.nodes.len() {
            return false;
        }
        let mut at = self.nodes[node].parent;
        let mut changed = false;
        while let Some(p) = at {
            changed |= self.expanded.insert(p);
            at = self.nodes[p].parent;
        }
        if changed {
            self.rebuild();
        }
        true
    }

    /// The row index of `node`, if it is visible.
    pub fn row_of(&self, node: usize) -> Option<usize> {
        self.rows.iter().position(|r| r.node == node)
    }

    /// Selects `node`, revealing it. Returns false for an unknown node.
    pub fn select(&mut self, node: usize) -> bool {
        if !self.reveal(node) {
            return false;
        }
        if self.selected != Some(node) {
            self.selected = Some(node);
            self.generation += 1;
        }
        true
    }

    fn root_of(&self, mut node: usize) -> usize {
        while let Some(p) = self.nodes[node].parent {
            node = p;
        }
        node
    }

    fn is_descendant(&self, node: usize, ancestor: usize) -> bool {
        let mut at = self.nodes.get(node).and_then(|n| n.parent);
        while let Some(p) = at {
            if p == ancestor {
                return true;
            }
            at = self.nodes[p].parent;
        }
        false
    }

    /// Applies a keyboard move to the selection.
    pub fn key(&mut self, key: TreeKey) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.selected.and_then(|s| self.row_of(s));
        let target = match (key, current) {
            (TreeKey::Home, _) | (TreeKey::Down, None) => Some(self.rows[0].node),
            (TreeKey::End, _) | (TreeKey::Up, None) => self.rows.last().map(|r| r.node),
            (TreeKey::Down, Some(i)) => self.rows.get(i + 1).map(|r| r.node),
            (TreeKey::Up, Some(i)) => i.checked_sub(1).map(|j| self.rows[j].node),
            (TreeKey::Right, Some(i)) => {
                let row = self.rows[i];
                if row.has_children && !row.expanded {
                    self.expand(row.node);
                    return;
                }
                if row.has_children {
                    self.nodes[row.node].children.first().copied()
                } else {
                    None
                }
            }
            (TreeKey::Left, Some(i)) => {
                let row = self.rows[i];
                if row.expanded {
                    self.collapse(row.node);
                    return;
                }
                self.nodes[row.node].parent
            }
            (TreeKey::Left | TreeKey::Right, None) => Some(self.rows[0].node),
        };
        if let Some(t) = target {
            self.select(t);
        }
    }

    /// Whether `source` may be dropped at `position` relative to `target`: not onto itself and not
    /// into its own subtree.
    pub fn can_drop(&self, source: usize, target: usize, position: DropPosition) -> bool {
        if source >= self.nodes.len() || target >= self.nodes.len() || source == target {
            return false;
        }
        if self.is_descendant(target, source) {
            return false;
        }
        // Dropping before or after the node that already sits there changes nothing but is allowed.
        let _ = position;
        true
    }

    /// Moves `source` (with its subtree) `position` relative to `target`: before or after it
    /// among its siblings, or inside it as its last child (expanding `target`). Returns whether
    /// the tree changed; a move [`Self::can_drop`] refuses changes nothing (ADR-2045).
    pub fn move_node(&mut self, source: usize, target: usize, position: DropPosition) -> bool {
        if !self.can_drop(source, target, position) {
            return false;
        }
        let depth_of = |t: &Self, mut n: usize| {
            let mut d = 0;
            while let Some(p) = t.nodes[n].parent {
                d += 1;
                n = p;
            }
            d
        };
        let subtree_height = {
            let mut best = 0;
            let mut stack = vec![(source, 0usize)];
            while let Some((n, d)) = stack.pop() {
                best = best.max(d);
                stack.extend(self.nodes[n].children.iter().map(|c| (*c, d + 1)));
            }
            best
        };
        let new_depth = match position {
            DropPosition::Inside => depth_of(self, target) + 1,
            _ => depth_of(self, target),
        };
        if new_depth + subtree_height >= MAX_DEPTH {
            return false;
        }
        // Detach.
        match self.nodes[source].parent {
            Some(p) => self.nodes[p].children.retain(|c| *c != source),
            None => self.roots.retain(|r| *r != source),
        }
        // Attach.
        match position {
            DropPosition::Inside => {
                self.nodes[target].children.push(source);
                self.nodes[source].parent = Some(target);
                self.expanded.insert(target);
            }
            DropPosition::Before | DropPosition::After => {
                let parent = self.nodes[target].parent;
                let list = match parent {
                    Some(p) => &mut self.nodes[p].children,
                    None => &mut self.roots,
                };
                let at = list.iter().position(|c| *c == target).unwrap_or(list.len());
                let at = if position == DropPosition::After {
                    at + 1
                } else {
                    at
                };
                list.insert(at.min(list.len()), source);
                self.nodes[source].parent = parent;
            }
        }
        self.rebuild();
        true
    }

    /// Classifies a pointer at fraction `y` (0 top, 1 bottom) of a row: the top quarter drops
    /// before, the bottom quarter after, the middle inside. A leaf row that cannot take children
    /// still reports `Inside`; the caller decides with [`Self::can_drop`].
    pub fn drop_position(y: f32) -> DropPosition {
        if !y.is_finite() || y < 0.25 {
            DropPosition::Before
        } else if y > 0.75 {
            DropPosition::After
        } else {
            DropPosition::Inside
        }
    }

    /// The list rows for the visible tree: indentation, a `[+]` / `[-]` marker and the label.
    pub fn list_items(&self) -> Vec<ListItem> {
        self.rows
            .iter()
            .map(|r| {
                let marker = match (r.has_children, r.expanded) {
                    (false, _) => "    ",
                    (true, false) => "[+] ",
                    (true, true) => "[-] ",
                };
                ListItem {
                    label: format!("{}{}{}", "    ".repeat(r.depth), marker, self.nodes[r.node].label),
                    subtitle: String::new(),
                }
            })
            .collect()
    }
}

impl UiTree {
    /// True once per change since the last call: the list sync systems use it to rewrite
    /// their rows only when the tree changed.
    pub fn take_change(&mut self) -> bool {
        if self.synced == self.generation {
            return false;
        }
        self.synced = self.generation;
        true
    }
}

/// `Update`: writes the visible rows of every changed [`UiTree`] into its [`ListItems`] (plain
/// text rows; drawn [`crate::parity_widgets::TreeView`] trees have their own sync) and marks
/// the list for rebinding so live rows show the new labels.
pub fn sync_tree_lists(
    mut trees: Query<
        (&mut UiTree, &mut ListItems, Option<&mut crate::scroll::ListView>),
        Without<crate::parity_widgets::TreeView>,
    >,
) {
    let _span = tracing::trace_span!("ui_tree_sync").entered();
    for (mut tree, mut items, list) in &mut trees {
        if tree.take_change() {
            items.0 = tree.list_items();
            if let Some(mut list) = list {
                list.dirty = true;
            }
        }
    }
}

/// One data grid column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridColumn {
    /// Header text.
    pub title: String,
    /// Width in UI px.
    pub width: f32,
}

/// A sortable table.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct DataGrid {
    /// Columns.
    pub columns: Vec<GridColumn>,
    /// Row cells, `columns.len()` per row (short rows read as empty cells).
    pub rows: Vec<Vec<String>>,
    /// The sort: column index and ascending.
    pub sort: Option<(usize, bool)>,
}

/// Compares two cells: numbers numerically, everything else case-insensitively; numbers sort before text.
fn compare_cells(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

impl DataGrid {
    /// Row indexes in display order (stable sort by the sort column).
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.rows.len()).collect();
        if let Some((col, asc)) = self.sort {
            let cell = |r: usize| self.rows[r].get(col).map_or("", String::as_str);
            order.sort_by(|a, b| {
                let o = compare_cells(cell(*a), cell(*b));
                if asc { o } else { o.reverse() }
            });
        }
        order
    }

    /// Sorts by `column`; sorting the same column again flips the direction. False for an unknown column.
    pub fn sort_by(&mut self, column: usize) -> bool {
        if column >= self.columns.len() {
            return false;
        }
        self.sort = match self.sort {
            Some((c, asc)) if c == column => Some((c, !asc)),
            _ => Some((column, true)),
        };
        true
    }

    /// The list rows: cells joined by a tab-like separator, in display order.
    pub fn list_items(&self) -> Vec<ListItem> {
        self.order()
            .into_iter()
            .map(|r| ListItem {
                label: self.rows[r].join("  |  "),
                subtitle: String::new(),
            })
            .collect()
    }
}

/// Counts of nodes per depth, for `ui.tree` summaries.
pub fn depth_histogram(tree: &UiTree) -> HashMap<usize, usize> {
    let mut map = HashMap::new();
    let mut stack: Vec<(usize, usize)> = tree.roots.iter().map(|r| (*r, 0)).collect();
    while let Some((id, d)) = stack.pop() {
        *map.entry(d).or_insert(0) += 1;
        stack.extend(tree.nodes[id].children.iter().map(|c| (*c, d + 1)));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(l: &str) -> TreeSpec {
        TreeSpec {
            label: l.into(),
            children: vec![],
        }
    }

    fn branch(l: &str, c: Vec<TreeSpec>) -> TreeSpec {
        TreeSpec {
            label: l.into(),
            children: c,
        }
    }

    fn sample() -> UiTree {
        UiTree::new(&[
            branch("a", vec![branch("a1", vec![leaf("a1x")]), leaf("a2")]),
            leaf("b"),
        ])
        .unwrap()
    }

    fn labels(t: &UiTree) -> Vec<&str> {
        t.rows().iter().map(|r| t.label(r.node).unwrap()).collect()
    }

    #[test]
    fn collapsed_by_default_and_expand_collapse() {
        let mut t = sample();
        assert_eq!(labels(&t), ["a", "b"]);
        assert!(t.expand(0));
        assert_eq!(labels(&t), ["a", "a1", "a2", "b"]);
        assert!(t.expand(1));
        assert_eq!(labels(&t), ["a", "a1", "a1x", "a2", "b"]);
        assert_eq!(t.rows()[2].depth, 2);
        assert!(!t.expand(1), "already expanded");
        assert!(!t.expand(4), "a leaf cannot expand");
        assert!(t.collapse(0));
        assert_eq!(labels(&t), ["a", "b"]);
        assert!(t.is_expanded(1), "inner state survives collapse");
        t.expand(0);
        assert_eq!(labels(&t), ["a", "a1", "a1x", "a2", "b"]);
    }

    #[test]
    fn expand_all_collapse_all_and_selection_follows() {
        let mut t = sample();
        t.expand_all();
        assert_eq!(t.rows().len(), 5);
        t.select(2);
        t.collapse_all();
        assert_eq!(
            t.selected,
            Some(0),
            "hidden selection moves to its top-level ancestor"
        );
        t.select(2);
        assert_eq!(labels(&t), ["a", "a1", "a1x", "a2", "b"], "select reveals");
        t.collapse(0);
        assert_eq!(t.selected, Some(0));
    }

    #[test]
    fn keyboard_navigation() {
        let mut t = sample();
        t.key(TreeKey::Down);
        assert_eq!(t.selected, Some(0));
        t.key(TreeKey::Right);
        assert!(t.is_expanded(0), "right expands");
        t.key(TreeKey::Right);
        assert_eq!(t.selected, Some(1), "right again enters the first child");
        t.key(TreeKey::Left);
        assert_eq!(
            t.selected,
            Some(0),
            "left on a collapsed child goes to the parent"
        );
        t.key(TreeKey::Left);
        assert!(!t.is_expanded(0), "left on an expanded node collapses");
        t.key(TreeKey::End);
        assert_eq!(t.selected, Some(4));
        t.key(TreeKey::Down);
        assert_eq!(t.selected, Some(4), "no row below the last");
        t.key(TreeKey::Home);
        assert_eq!(t.selected, Some(0));
        t.key(TreeKey::Up);
        assert_eq!(t.selected, Some(0));
    }

    #[test]
    fn drop_rules() {
        let t = sample();
        assert_eq!(UiTree::drop_position(0.1), DropPosition::Before);
        assert_eq!(UiTree::drop_position(0.5), DropPosition::Inside);
        assert_eq!(UiTree::drop_position(0.9), DropPosition::After);
        assert_eq!(UiTree::drop_position(f32::NAN), DropPosition::Before);
        assert!(t.can_drop(4, 0, DropPosition::Inside));
        assert!(!t.can_drop(0, 0, DropPosition::Inside), "not onto itself");
        assert!(
            !t.can_drop(0, 2, DropPosition::Inside),
            "not into its own subtree"
        );
        assert!(!t.can_drop(99, 0, DropPosition::Before));
    }

    #[test]
    fn list_items_indent_and_mark() {
        let mut t = sample();
        t.expand(0);
        let items = t.list_items();
        assert_eq!(items[0].label, "[-] a");
        assert_eq!(items[1].label, "    [+] a1");
        assert_eq!(items[2].label, "        a2");
    }

    #[test]
    fn ten_thousand_nodes_expand_all() {
        let kids: Vec<TreeSpec> = (0..100)
            .map(|i| {
                branch(
                    &format!("g{i}"),
                    (0..99).map(|j| leaf(&format!("n{i}_{j}"))).collect(),
                )
            })
            .collect();
        let mut t = UiTree::new(&kids).unwrap();
        assert_eq!(t.len(), 100 + 100 * 99);
        assert_eq!(t.rows().len(), 100);
        t.expand_all();
        assert_eq!(t.rows().len(), 10_000);
        assert_eq!(depth_histogram(&t)[&1], 9900);
    }

    #[test]
    fn limits_are_enforced() {
        let mut deep = leaf("x");
        for _ in 0..MAX_DEPTH {
            deep = branch("x", vec![deep]);
        }
        assert_eq!(UiTree::new(&[deep]), Err(TreeError::TooDeep));
    }

    #[test]
    fn sync_writes_list_items_once_per_change() {
        let mut app = App::new();
        app.add_systems(Update, sync_tree_lists);
        let mut tree = sample();
        tree.expand(0);
        let e = app.world_mut().spawn((tree, ListItems::default())).id();
        app.update();
        assert_eq!(app.world().get::<ListItems>(e).unwrap().0.len(), 4);
        app.world_mut().get_mut::<ListItems>(e).unwrap().0.clear();
        app.update();
        assert!(
            app.world().get::<ListItems>(e).unwrap().0.is_empty(),
            "unchanged tree is not rewritten"
        );
        app.world_mut().get_mut::<UiTree>(e).unwrap().expand_all();
        app.update();
        assert_eq!(app.world().get::<ListItems>(e).unwrap().0.len(), 5);
    }

    #[test]
    fn grid_sorts_numbers_then_text_and_flips() {
        let mut g = DataGrid {
            columns: vec![
                GridColumn {
                    title: "name".into(),
                    width: 100.0,
                },
                GridColumn {
                    title: "hp".into(),
                    width: 50.0,
                },
            ],
            rows: vec![
                vec!["orc".into(), "30".into()],
                vec!["Elf".into(), "9".into()],
                vec!["bat".into(), "-".into()],
                vec!["ant".into()],
            ],
            sort: None,
        };
        assert_eq!(g.order(), [0, 1, 2, 3]);
        assert!(g.sort_by(0));
        assert_eq!(g.order(), [3, 2, 1, 0], "case-insensitive text");
        assert!(g.sort_by(0));
        assert_eq!(g.order(), [0, 1, 2, 3], "same column flips");
        assert!(g.sort_by(1));
        assert_eq!(
            g.order(),
            [1, 0, 3, 2],
            "numbers first (9, 30), then text by order (empty before -)"
        );
        assert!(!g.sort_by(7));
        assert_eq!(g.list_items().len(), 4);
    }

    #[test]
    fn move_node_reorders_reparents_and_refuses_cycles() {
        let mut t = UiTree::new(&[branch("a", vec![leaf("a1"), leaf("a2")]), leaf("b")]).unwrap();
        t.expand_all();
        let labels = |t: &UiTree| -> Vec<String> {
            t.rows()
                .iter()
                .map(|r| format!("{}{}", "-".repeat(r.depth), t.label(r.node).unwrap_or("")))
                .collect()
        };
        assert_eq!(labels(&t), ["a", "-a1", "-a2", "b"]);
        // a2 before a1; b inside a; a into its own child refused.
        assert!(t.move_node(2, 1, DropPosition::Before));
        assert_eq!(labels(&t), ["a", "-a2", "-a1", "b"]);
        assert!(t.move_node(3, 0, DropPosition::Inside));
        assert_eq!(labels(&t), ["a", "-a2", "-a1", "-b"]);
        assert!(!t.move_node(0, 1, DropPosition::Inside));
        assert!(!t.move_node(1, 1, DropPosition::After));
        // a1 out to the root level after a.
        assert!(t.move_node(1, 0, DropPosition::After));
        assert_eq!(labels(&t), ["a", "-a2", "-b", "a1"]);
        assert_eq!(t.parent(1), None);
    }
}
